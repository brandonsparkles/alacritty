//! Per-tool resume-command resolver for the macOS native-tab session
//! restoration path.
//!
//! Each supported AI CLI stores some piece of metadata that lets us identify
//! *which* conversation a given running process owns — critical when the
//! user has multiple tabs in the same cwd each carrying a different
//! conversation. Identifying the session per-tab (rather than just "the
//! newest one in this cwd") is the difference between true tab restoration
//! and a multi-tab merge into one stream.
//!
//! ## Supported tools
//!
//! | Tool                 | Identifier source                              | Resume command                        |
//! |----------------------|------------------------------------------------|---------------------------------------|
//! | claude               | `~/.claude/sessions/<pid>.json` → `sessionId`,  | `claude <flags> --resume <id>`        |
//! |                      | or `~/.claude-accounts/<label>/sessions/…`     | (`CLAUDE_CONFIG_DIR` prefix when the  |
//! |                      |                                                | session lives in an account store)    |
//! | cglm/cqwn/cmo/cfu/   | Same markers; provider endpoint in argv        | `<wrapper> [--flash] [--account <lbl>]|
//! | crt/cor/coc          | `--settings` selects the wrapper               | <flags> --resume <id>`                |
//! | copilot              | `~/.copilot/logs/process-<ts>-<pid>.log` last  | `copilot <flags> --resume=<id>`       |
//! |                      | "Registering foreground session: <uuid>" entry |                                       |
//! | codex / codexpilot   | argv `resume <uuid>`, else the rollout         | `<program> <flags> resume <id>`       |
//! |                      | `.jsonl` the process (or its managed daemon)   |                                       |
//! |                      | holds OPEN                                     |                                       |
//! | opencode             | argv `--session <id>`, else the newest         | `ooc <flags> --session <id>`          |
//! |                      | unclaimed top-level `session_v2` row           |                                       |
//!
//! Tools are matched by their resolved binary path (via `proc_pidpath`),
//! falling back to the kernel's saved launch path after an upgrade unlinks the
//! running binary. `pbi_comm` is truncated to 15 chars and reflects
//! the symlink target's basename (e.g. claude shows as `"2.1.144"`).
//!
//! ## Codex caveat
//!
//! codex stores sessions as `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`
//! and writes no PID-keyed marker. If codex was started by our restore command,
//! argv contains `resume <uuid>` and wins; otherwise we read the rollout out of
//! the process's open-file table.
//!
//! We previously joined PID→session by start-time proximity (rollout timestamp
//! within 10 minutes after `pbi_start_tvsec`, in the process-start date dir).
//! That silently resolved almost nothing in practice: **resuming a conversation
//! appends to its ORIGINAL rollout file**, so a session picked from codex's
//! in-TUI history carries a filename timestamp from the day it was born — days
//! or weeks before the current process — failing both the date-dir lookup and
//! the "at or after process start" bound. Only a brand-new conversation that
//! got its first prompt within 10 minutes of launch ever matched, which is why
//! a multi-tab restore came back with at most one live codex session.
//!
//! We deliberately do not persist `resume --last` because it makes multiple
//! restored tabs open the same most-recent conversation.

#![cfg(target_os = "macos")]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::raw::c_int;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::config::ai_resume::AiResumeConfig;

/// Session identifiers claimed by sibling windows during the current
/// save-tick (codex rollout UUIDs and opencode session ids share this set).
/// Cleared by `begin_save_tick()` from the save worker before each iteration
/// over windows. Ensures two tabs whose PIDs started in the same second don't
/// both resolve to the same candidate — the first window to call grabs it,
/// the second gets the next best unclaimed one.
static CLAIMED_SESSION_IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn claimed_ids() -> &'static Mutex<HashSet<String>> {
    CLAIMED_SESSION_IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Clear the per-save-tick claim set. Call once at the start of each
/// `save_session` iteration over windows.
pub fn begin_save_tick() {
    if let Ok(mut g) = claimed_ids().lock() {
        g.clear();
    }
}

/// Resolve a resume command for any allowlisted AI CLI in the process
/// subtree rooted at `shell_pid`. Walks up to `MAX_DEPTH` levels because
/// the user-facing CLI is often a node-wrapper script whose actual binary
/// sits two or three forks deeper (e.g. zsh → `node /opt/homebrew/bin/copilot`
/// → `@github/copilot-darwin-arm64/copilot`). First match wins.
pub fn resume_command_for(
    shell_pid: c_int,
    _shell_cwd: &Path,
    ai_resume: &AiResumeConfig,
) -> Option<String> {
    /// Bounded BFS depth — avoids runaway traversal into deep subtrees.
    /// Empirically the shell→wrapper→binary chain is depth 2; we allow a
    /// little slack for future layering.
    const MAX_DEPTH: usize = 4;

    let mut frontier: Vec<c_int> = vec![shell_pid];
    let mut visited: std::collections::HashSet<c_int> = std::collections::HashSet::new();
    visited.insert(shell_pid);
    for depth in 0..MAX_DEPTH {
        let mut next_frontier: Vec<c_int> = Vec::new();
        for parent in &frontier {
            for child in crate::macos::proc::list_children(*parent) {
                if !visited.insert(child) {
                    continue;
                }
                let path = match crate::macos::proc::pid_path(child) {
                    Some(p) => p,
                    None => {
                        next_frontier.push(child);
                        continue;
                    },
                };
                let Some(path_str) = path.to_str() else {
                    next_frontier.push(child);
                    continue;
                };
                let _ = depth; // diagnostic-only; kept for future logging
                if is_claude_binary(path_str) {
                    if let Some(cmd) = claude_resume(child, ai_resume) {
                        return Some(cmd);
                    }
                } else if is_copilot_binary(path_str) {
                    if let Some(cmd) = copilot_resume(child, ai_resume) {
                        return Some(cmd);
                    }
                } else if let Some(program) = codex_program_for_binary(path_str) {
                    if let Some(cmd) = codex_resume(child, _shell_cwd, ai_resume, program) {
                        return Some(cmd);
                    }
                } else if is_opencode_binary(path_str) {
                    if let Some(cmd) = opencode_resume(child, _shell_cwd, ai_resume) {
                        return Some(cmd);
                    }
                }
                // Not an AI binary at this level — keep traversing in case
                // it's a node/python wrapper that spawned one.
                next_frontier.push(child);
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
    None
}

// ---------- claude ----------

/// Claude's auto-updater prunes old `versions/<x.y.z>` binaries while
/// long-idle sessions still run them. `pid_path` then falls back to the
/// launch path saved in `KERN_PROCARGS2`, which is the unresolved
/// `~/.local/bin/claude` symlink, so a bare `claude` file name also counts.
/// A false positive costs nothing: `claude_resume` still requires the
/// per-PID `sessions/<pid>.json` marker.
fn is_claude_binary(p: &str) -> bool {
    p.contains("/.claude/versions/")
        || p.contains("/claude/versions/")
        || Path::new(p).file_name().is_some_and(|name| name == "claude")
}

/// claude maintains `sessions/<pid>.json` with `sessionId` while the process
/// is alive. Search the stock `~/.claude` store first, then the `crt` account
/// stores under `~/.claude-accounts/<label>` — each store is a separate
/// CLAUDE_CONFIG_DIR with its own login, and the one that holds the marker is
/// the one a replayed `--resume` must bind to.
fn claude_session_meta(pid: c_int) -> Option<(String, Option<String>)> {
    let mut stock = home::home_dir()?;
    stock.push(".claude");
    stock.push("sessions");
    if let Some(id) = claude_session_id_from_marker(&stock.join(format!("{pid}.json"))) {
        return Some((id, None));
    }
    let mut accounts = home::home_dir()?;
    accounts.push(".claude-accounts");
    for entry in std::fs::read_dir(accounts).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let marker = dir.join("sessions").join(format!("{pid}.json"));
        if let Some(id) = claude_session_id_from_marker(&marker) {
            return Some((id, dir.to_str().map(str::to_string)));
        }
    }
    None
}

/// Minimal-parse one `sessions/<pid>.json` marker for its `sessionId`.
fn claude_session_id_from_marker(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let session_id = v.get("sessionId")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    Some(session_id.to_string())
}

fn claude_resume(pid: c_int, ai_resume: &AiResumeConfig) -> Option<String> {
    let (session_id, config_dir) = claude_session_meta(pid)?;
    let args = crate::macos::proc::argv(pid)?;
    claude_resume_from_args(&session_id, config_dir.as_deref(), &args, ai_resume)
}

fn claude_resume_command(
    session_id: &str,
    config_dir: Option<&str>,
    ai_resume: &AiResumeConfig,
) -> String {
    let core = build_command("claude", &ai_resume.claude.flags, ["--resume", session_id]);
    with_config_dir_prefix(core, config_dir)
}

/// The claude launcher shell functions in ~/.zshrc exec the same binary but
/// carry their own provider endpoint in inline --settings. Preserve that
/// launcher instead of silently resuming with subscription credentials.
/// Never persist auth tokens.
fn claude_resume_from_args(
    session_id: &str,
    config_dir: Option<&str>,
    args: &[String],
    ai_resume: &AiResumeConfig,
) -> Option<String> {
    let (endpoint, flash) = claude_route_from_args(args);
    claude_command_for(session_id, endpoint.as_deref(), flash, config_dir, ai_resume)
}

/// Extract the provider endpoint and flash-model flag from claude argv.
fn claude_route_from_args<S: AsRef<str>>(args: &[S]) -> (Option<String>, bool) {
    let mut endpoint = None;
    let mut flash = false;
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        let arg = arg.as_ref();
        let settings = if arg == "--settings" {
            args.next().map(|value| value.as_ref())
        } else {
            arg.strip_prefix("--settings=")
        };
        if let Some(settings) = settings {
            // Persisted commands re-parse shell-quoted tokens; live argv is
            // raw. Unquoting is a no-op on the latter.
            let settings = unshell_quote(settings);
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&settings) {
                if let Some(url) = value
                    .get("env")
                    .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                    .and_then(|url| url.as_str())
                {
                    endpoint = Some(url.to_owned());
                }
            }
            continue;
        }
        let model = if arg == "--model" {
            args.next().map(|value| value.as_ref())
        } else {
            arg.strip_prefix("--model=")
        };
        if let Some(model) = model {
            flash = model == "haiku";
        }
    }
    (endpoint, flash)
}

/// Strip one layer of `shell_quote` quoting. Identity for raw argv tokens.
fn unshell_quote(arg: &str) -> String {
    if arg.len() >= 2 && arg.starts_with('\'') && arg.ends_with('\'') {
        return arg[1..arg.len() - 1].replace("'\\''", "'");
    }
    arg.to_string()
}

/// A claude-launcher shell function from ~/.zshrc. Wrappers own their own
/// auth env (tokens, model maps, config-dir pinning), so replaying through the
/// wrapper keeps session.json free of auth material while still reopening the
/// conversation against the right provider. `flash` = the wrapper consumes a
/// leading `--flash`; `account` = the wrapper pins a `crt` account store via
/// `--account <label>`. Endpoints must track the `ANTHROPIC_BASE_URL` values
/// hardcoded in the zshrc wrappers.
struct ClaudeWrapper {
    name: &'static str,
    flash: bool,
    account: bool,
}

const CLAUDE_WRAPPERS: &[(&str, &str, bool, bool)] = &[
    ("cglm", "https://api.z.ai/api/anthropic", true, false),
    (
        "cqwn",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
        true,
        false,
    ),
    ("cmo", "https://token-plan-sgp.xiaomimimo.com/anthropic", true, false),
    ("cfu", "http://127.0.0.1:8081", true, false),
    ("crt", "http://127.0.0.1:8082", false, true),
    ("cor", "https://openrouter.ai/api", false, false),
    ("coc", "http://127.0.0.1:8080", false, false),
];

fn claude_wrapper_for_endpoint(endpoint: &str) -> Option<ClaudeWrapper> {
    CLAUDE_WRAPPERS
        .iter()
        .find(|(_, url, _, _)| *url == endpoint)
        .map(|(name, _, flash, account)| ClaudeWrapper {
            name,
            flash: *flash,
            account: *account,
        })
}

fn claude_wrapper_for_program(program: &str) -> Option<ClaudeWrapper> {
    CLAUDE_WRAPPERS
        .iter()
        .find(|(name, _, _, _)| *name == program)
        .map(|(name, _, flash, account)| ClaudeWrapper {
            name,
            flash: *flash,
            account: *account,
        })
}

/// `crt --account` labels are the store-dir basenames (`~/.claude-accounts/acct3`
/// → `acct3`); the stock `~/.claude` store pins as `default`.
fn claude_account_label(config_dir: &str) -> Option<&str> {
    Path::new(config_dir).file_name().and_then(|n| n.to_str())
}

/// Prefix a built command with a `CLAUDE_CONFIG_DIR` assignment so a session
/// living in a non-default account store resumes against that store's login.
fn with_config_dir_prefix(core: String, config_dir: Option<&str>) -> String {
    match config_dir {
        Some(dir) => format!("CLAUDE_CONFIG_DIR={} {}", shell_quote(dir), core),
        None => core,
    }
}

/// Rebuild the resume command for a claude session given its provider route.
fn claude_command_for(
    session_id: &str,
    endpoint: Option<&str>,
    flash: bool,
    config_dir: Option<&str>,
    ai_resume: &AiResumeConfig,
) -> Option<String> {
    let endpoint = endpoint.map(|url| url.trim_end_matches('/'));
    if matches!(endpoint, None | Some("") | Some("https://api.anthropic.com")) {
        return Some(claude_resume_command(session_id, config_dir, ai_resume));
    }
    let url = endpoint?;
    if let Some(wrapper) = claude_wrapper_for_endpoint(url) {
        // `crt` pins the account store explicitly via `--account`; the other
        // wrappers inherit it through a `CLAUDE_CONFIG_DIR` prefix.
        let account = if wrapper.account {
            Some(claude_account_label(config_dir.unwrap_or("")).unwrap_or("default"))
        } else {
            config_dir.and_then(claude_account_label)
        };
        return Some(claude_wrapper_command(
            &wrapper,
            session_id,
            flash,
            account,
            config_dir,
            ai_resume,
        ));
    }
    // A custom provider must never fall back to paid Claude credits.
    // Persist the endpoint URL alone — the rest of --settings can carry
    // auth tokens or tuning env that must not leak into session.json.
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(claude_resume_command_with_endpoint(
            session_id,
            url,
            config_dir,
            ai_resume,
        ));
    }
    None
}

fn claude_resume_command_with_endpoint(
    session_id: &str,
    endpoint: &str,
    config_dir: Option<&str>,
    ai_resume: &AiResumeConfig,
) -> String {
    // URL-only settings rebuild: anything else the live process carried
    // (tokens, tool-search toggles, concurrency caps) is deliberately dropped.
    let settings =
        serde_json::json!({ "env": { "ANTHROPIC_BASE_URL": endpoint } }).to_string();
    let core = build_command(
        "claude",
        &ai_resume.claude.flags,
        ["--settings", &settings, "--resume", session_id],
    );
    with_config_dir_prefix(core, config_dir)
}

/// Replay through the originating zsh wrapper, which re-derives its own
/// auth env. `account` is the `crt` label (or `default`).
fn claude_wrapper_command(
    wrapper: &ClaudeWrapper,
    session_id: &str,
    flash: bool,
    account: Option<&str>,
    config_dir: Option<&str>,
    ai_resume: &AiResumeConfig,
) -> String {
    // `crt` must see --account before any forwarded Claude flags; other
    // wrappers parse a leading `--flash` the same way. Config flags come last
    // so both parsers stay happy.
    let head: Vec<&str> = if wrapper.account {
        account.map(|label| vec!["--account", label]).unwrap_or_default()
    } else {
        Vec::new()
    };
    let tail = head
        .into_iter()
        .chain((flash && wrapper.flash).then_some("--flash"))
        .chain(ai_resume.claude.flags.iter().map(String::as_str))
        .chain(["--resume", session_id]);
    let core = build_command(wrapper.name, &[], tail);
    if wrapper.account {
        return core;
    }
    with_config_dir_prefix(core, config_dir)
}

// ---------- copilot ----------

fn is_copilot_binary(p: &str) -> bool {
    p.contains("@github/copilot/")
        || p.contains("/copilot-darwin-arm64/copilot")
        || p.contains("/copilot-darwin-x64/copilot")
}

/// Copilot writes a per-process log at `~/.copilot/logs/process-<ts>-<pid>.log`
/// and emits `Registering foreground session: <uuid>` whenever it activates
/// a session. The LAST such entry is the currently-active session.
fn copilot_resume(pid: c_int, ai_resume: &AiResumeConfig) -> Option<String> {
    let mut logs_dir = home::home_dir()?;
    logs_dir.push(".copilot");
    logs_dir.push("logs");

    let pid_suffix = format!("-{}.log", pid);
    let mut matched: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(logs_dir).ok()?.flatten() {
        let p = entry.path();
        let Some(fname) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if fname.starts_with("process-") && fname.ends_with(&pid_suffix) {
            matched = Some(p);
            break;
        }
    }
    let log = matched?;

    let uuid = copilot_scan_uuid(pid, &log)?;
    Some(copilot_resume_command(&uuid, ai_resume))
}

/// Incremental scan state for one copilot per-process log: byte offset of
/// the first unread line plus the last session UUID seen so far. Keyed by
/// PID; bounded by the number of distinct copilot processes seen during
/// this alacritty run. `dev`/`ino` identify the log file the offset was
/// measured against, so a recycled PID pointing at a different log is
/// detected and rescanned from the start (spk-961c23c40386).
struct CopilotScanState {
    offset: u64,
    last_uuid: Option<String>,
    dev: u64,
    ino: u64,
}

static COPILOT_SCAN_STATES: OnceLock<Mutex<HashMap<c_int, CopilotScanState>>> = OnceLock::new();

/// Tail-offset incremental read of a copilot log: only bytes appended
/// since the previous scan are parsed, so a long-lived session doesn't
/// re-read a growing log end-to-end on every save tick. A file shorter
/// than the stored offset (rotation/truncation) resets the scan; a
/// trailing partial line is left unconsumed for the next pass.
fn copilot_scan_uuid(pid: c_int, log: &Path) -> Option<String> {
    const MARKER: &str = "Registering foreground session: ";
    let states = COPILOT_SCAN_STATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut states = states.lock().ok()?;

    let mut f = File::open(log).ok()?;
    let meta = f.metadata().ok()?;
    let len = meta.len();
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    let state =
        states.entry(pid).or_insert(CopilotScanState { offset: 0, last_uuid: None, dev, ino });
    if state.dev != dev || state.ino != ino {
        // The PID was recycled (or the log replaced): the stored offset
        // belongs to a different file. Rescan from the start rather than
        // tailing mid-file past the session marker.
        state.offset = 0;
        state.last_uuid = None;
        state.dev = dev;
        state.ino = ino;
    }
    if len < state.offset {
        state.offset = 0;
        state.last_uuid = None;
    }
    if len > state.offset {
        f.seek(SeekFrom::Start(state.offset)).ok()?;
        let mut appended = Vec::with_capacity((len - state.offset) as usize);
        f.take(len - state.offset).read_to_end(&mut appended).ok()?;
        // Consume only complete lines; offsets are tracked in raw bytes so
        // lossy UTF-8 decoding can't skew them.
        let consumed = appended.iter().rposition(|&b| b == b'\n').map_or(0, |idx| idx + 1);
        if consumed > 0 {
            for line in String::from_utf8_lossy(&appended[..consumed]).lines() {
                if let Some(idx) = line.find(MARKER) {
                    let tail = line[idx + MARKER.len()..].trim();
                    if !tail.is_empty() {
                        state.last_uuid = Some(tail.to_string());
                    }
                }
            }
            state.offset += consumed as u64;
        }
    }
    state.last_uuid.clone()
}

fn copilot_resume_command(session_id: &str, ai_resume: &AiResumeConfig) -> String {
    build_command(
        "copilot",
        &copilot_resume_flags(&ai_resume.copilot.flags),
        [format!("--resume={session_id}")],
    )
}

fn copilot_resume_flags(flags: &[String]) -> Vec<String> {
    if flags
        .iter()
        .any(|flag| flag == "--mouse" || flag.starts_with("--mouse=") || flag == "--no-mouse")
    {
        return flags.to_vec();
    }

    // Copilot uses alt-screen; without mouse mode wheel events degrade to
    // arrow-key history navigation under Alacritty's standard alternate-scroll behavior.
    let mut flags = flags.to_vec();
    flags.push("--mouse=on".into());
    flags
}

// ---------- codex ----------

fn codex_program_for_binary(p: &str) -> Option<&'static str> {
    if p.contains("/codexpilot/") || p.contains("/codexpilot-") {
        return Some("codexpilot");
    }
    if p.contains("@openai/codex") || p.contains("/codex-darwin-") {
        return Some("codex");
    }
    None
}

/// codex doesn't write PID-keyed session metadata anywhere, but it keeps the
/// rollout `.jsonl` of every session it is serving OPEN:
///
///   `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<iso-ts>-<session-uuid>.jsonl`
///
/// So we read the process's open-file table instead of guessing from
/// timestamps. Returns `None` when no session can be recovered. Replaying
/// `codex resume --last` is intentionally avoided because it makes multiple
/// restored tabs collapse into the same newest conversation.
fn codex_resume(
    pid: c_int,
    cwd: &Path,
    ai_resume: &AiResumeConfig,
    program: &str,
) -> Option<String> {
    if let Some(uuid) = codex_resume_arg(pid) {
        return Some(codex_resume_command(program, &uuid, ai_resume));
    }
    if let Some(uuid) = codex_session_for_pid(pid, cwd) {
        return Some(codex_resume_command(program, &uuid, ai_resume));
    }
    None
}

fn codex_resume_command(program: &str, session_id: &str, ai_resume: &AiResumeConfig) -> String {
    build_command(program, &ai_resume.codex.flags, ["resume", session_id])
}

fn codex_resume_arg(pid: c_int) -> Option<String> {
    let args = crate::macos::proc::argv(pid)?;
    codex_resume_arg_from(&args)
}

fn codex_resume_arg_from(args: &[String]) -> Option<String> {
    for pair in args.windows(2) {
        if pair[0] == "resume" && pair[1] != "--last" && !pair[1].starts_with('-') {
            return Some(pair[1].clone());
        }
    }
    None
}

/// Upgrade a persisted resume command to the current flag policy.
///
/// Session files outlive the code that wrote them, so replaying the stored
/// string verbatim can resurrect old flag sets. Normalize on load instead.
pub fn normalize_saved_resume_command(command: &str, ai_resume: &AiResumeConfig) -> Option<String> {
    let mut args: Vec<&str> = command.split_whitespace().collect();
    // A persisted `CLAUDE_CONFIG_DIR=…` assignment binds the replay to a `crt`
    // account store. Pull leading NAME=VALUE tokens off so the program is the
    // first token again. (Store paths live under $HOME and are whitespace-free.)
    let mut config_dir: Option<String> = None;
    while let Some(first) = args.first().copied() {
        let Some((name, value)) = first.split_once('=') else { break };
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            break;
        }
        if name == "CLAUDE_CONFIG_DIR" {
            config_dir = Some(unshell_quote(value));
        }
        args.remove(0);
    }
    let program = *args.first()?;
    if let Some(wrapper) = claude_wrapper_for_program(program) {
        let id = claude_resume_id_from_args(&args)?;
        let flash = wrapper.flash && args.contains(&"--flash");
        let mut account = args
            .windows(2)
            .find(|w| w[0] == "--account")
            .and_then(|w| w.get(1).map(|label| label.to_string()));
        if wrapper.account {
            let label = account
                .or_else(|| {
                    config_dir
                        .as_deref()
                        .and_then(claude_account_label)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "default".to_string());
            account = Some(label);
        }
        return Some(claude_wrapper_command(
            &wrapper,
            &id,
            flash,
            account.as_deref(),
            config_dir.as_deref(),
            ai_resume,
        ));
    }
    if program.ends_with("claude") {
        // Keep the provider endpoint across save/load: a custom-router session
        // must not be normalized back onto subscription credentials.
        let id = claude_resume_id_from_args(&args)?;
        let (endpoint, flash) = claude_route_from_args(&args);
        return claude_command_for(&id, endpoint.as_deref(), flash, config_dir.as_deref(), ai_resume);
    }
    if program.ends_with("copilot") {
        return copilot_resume_id_from_args(&args).map(|id| copilot_resume_command(&id, ai_resume));
    }
    if program.ends_with("codexpilot") {
        return codex_resume_id_from_args(&args)
            .map(|id| codex_resume_command("codexpilot", &id, ai_resume));
    }
    if program.ends_with("codex") {
        return codex_resume_id_from_args(&args)
            .map(|id| codex_resume_command("codex", &id, ai_resume));
    }
    if program == "ooc" || program.ends_with("opencode") {
        return opencode_session_id_from_args(&args)
            .map(|id| opencode_resume_command(&id, ai_resume));
    }
    None
}

fn build_command<I, S>(program: &str, flags: &[String], tail: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    std::iter::once(program.to_string())
        .chain(flags.iter().cloned())
        .chain(tail.into_iter().map(|part| part.as_ref().to_string()))
        .map(|part| shell_quote(&part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'=' | b'+')
        })
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn claude_resume_id_from_args(args: &[&str]) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        if *arg == "--resume" || *arg == "-r" {
            return args.get(idx + 1).filter(|id| !id.starts_with('-')).map(|id| id.to_string());
        }
        if let Some(id) = arg.strip_prefix("--resume=") {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn copilot_resume_id_from_args(args: &[&str]) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        if *arg == "--resume" {
            return args.get(idx + 1).filter(|id| !id.starts_with('-')).map(|id| id.to_string());
        }
        if let Some(id) = arg.strip_prefix("--resume=") {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn codex_resume_id_from_args(args: &[&str]) -> Option<String> {
    for pair in args.windows(2) {
        if pair[0] == "resume" && pair[1] != "--last" && !pair[1].starts_with('-') {
            return Some(pair[1].to_string());
        }
    }
    None
}

fn codex_session_for_pid(pid: c_int, cwd: &Path) -> Option<String> {
    // codex 0.162+ moves rollout fds onto its per-user managed app-server
    // daemon (`codex app-server --managed-daemon`, PPID 1) — the matched TUI
    // binary holds none, and the daemon is unreachable through descendants.
    // Merge (1) the TUI's own fds (pre-0.162), (2) near descendants, and
    // (3) the daemon's fd table from ~/.codex/app-server-daemon/daemon.pid,
    // so every layout resolves.
    let mut open_paths = crate::macos::proc::open_file_paths(pid);
    for child in crate::macos::proc::list_descendants(pid, 2) {
        open_paths.extend(crate::macos::proc::open_file_paths(child));
    }
    if let Some(daemon) = codex_daemon_pid() {
        if daemon != pid {
            open_paths.extend(crate::macos::proc::open_file_paths(daemon));
        }
    }
    codex_session_from_open_paths(&open_paths, cwd.to_str().unwrap_or(""))
}

/// The managed-daemon PID recorded in `~/.codex/app-server-daemon/daemon.pid`.
fn codex_daemon_pid() -> Option<c_int> {
    let mut p = home::home_dir()?;
    p.push(".codex");
    p.push("app-server-daemon");
    p.push("daemon.pid");
    let raw = std::fs::read_to_string(p).ok()?;
    parse_codex_daemon_pid(&raw)
}

/// Parse a daemon.pid record. `processIdentity.startSeconds` must match the
/// live process's start time, so a stale record pointing at a recycled PID
/// (e.g. across reboots) is never trusted.
fn parse_codex_daemon_pid(raw: &str) -> Option<c_int> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let pid = v.get("pid")?.as_i64()?;
    if pid <= 0 || pid > i32::MAX as i64 {
        return None;
    }
    let pid = pid as c_int;
    let expected_start = v
        .get("processIdentity")
        .and_then(|id| id.get("startSeconds"))
        .and_then(|s| s.as_i64());
    if let Some(expected) = expected_start {
        if crate::macos::proc::start_tvsec(pid) != Some(expected as u64) {
            return None;
        }
    }
    Some(pid)
}

/// Testable inner implementation. Takes the already-collected list of open
/// file paths so tests can supply fixtures without calling into the kernel.
///
/// A busy codex process holds MANY rollouts open at once: one per in-flight
/// subagent thread plus the user's own conversation. Only the latter is
/// meaningful to resume, and it is distinguishable — subagent rollouts carry
/// `payload.source.subagent`, the user session carries `source: "cli"`.
///
/// Ranking among surviving candidates is `(cwd match, mtime)`, highest wins.
/// Unlike the old timestamp matcher, a cwd mismatch only DEMOTES a candidate
/// rather than rejecting it: the open fd already proves this process owns the
/// session, so a user who `cd`s before launching codex (making the tab's cwd
/// differ from the recorded session cwd) should still get their conversation
/// back. mtime breaks the remaining tie toward the most recently active
/// conversation, which is the right answer after an in-process `/new`.
/// Under the daemon layout every tab's rollouts share one fd pool, so with
/// several concurrently-active tabs the tab↔rollout pairing is a best-effort
/// claim (recency + the claim set); the pairing becomes exact as soon as the
/// replayed `resume <uuid>` argv pins it.
fn codex_session_from_open_paths(open_paths: &[std::path::PathBuf], cwd: &str) -> Option<String> {
    // IDs already taken by a sibling window in this save-tick are skipped.
    // Under the daemon layout sibling tabs share one fd pool, so claiming is
    // what keeps two tabs from persisting the same id.
    let claimed: HashSet<String> = claimed_ids().lock().map(|g| g.clone()).unwrap_or_default();

    let mut best: Option<((bool, i64), String)> = None;
    for path in open_paths {
        let Some(uuid) = rollout_uuid_from_path(path) else { continue };
        if claimed.contains(&uuid) {
            continue;
        }
        let Some(meta) = rollout_session_meta(path) else { continue };
        if meta.is_subagent {
            continue;
        }
        let rank = (meta.cwd == cwd, rollout_mtime(path));
        if best.as_ref().is_none_or(|(best_rank, _)| *best_rank < rank) {
            best = Some((rank, uuid));
        }
    }

    let uuid = best.map(|(_, uuid)| uuid)?;
    if let Ok(mut g) = claimed_ids().lock() {
        g.insert(uuid.clone());
    }
    Some(uuid)
}

/// Extract the session UUID from a `rollout-<iso-ts>-<uuid>.jsonl` path.
/// `None` for any path that isn't a codex rollout.
fn rollout_uuid_from_path(path: &Path) -> Option<String> {
    // Guard on the containing `.codex/sessions` tree so an unrelated file that
    // happens to be named `rollout-…jsonl` can't be mistaken for a session.
    if !path.to_str()?.contains("/.codex/sessions/") {
        return None;
    }
    let fname = path.file_name()?.to_str()?;
    if !fname.starts_with("rollout-") || !fname.ends_with(".jsonl") {
        return None;
    }
    // `rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl` — 19-char timestamp, then the uuid.
    let body = &fname["rollout-".len()..fname.len() - ".jsonl".len()];
    if body.len() <= 20 {
        return None;
    }
    Some(body[20..].to_string())
}

/// Last-modified time in whole seconds, or 0 when unreadable. Used only for
/// ranking, so a missing value just sorts last.
fn rollout_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct RolloutMeta {
    cwd: String,
    is_subagent: bool,
}

/// Parse the leading `session_meta` record of a rollout.
fn rollout_session_meta(path: &Path) -> Option<RolloutMeta> {
    let f = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    let payload = v.get("payload")?;
    // Subagent rollouts carry `source: {"subagent": {...}}`; user-initiated
    // sessions carry `source: "cli"`.
    let is_subagent = payload
        .get("source")
        .is_some_and(|source| source.is_object() && source.get("subagent").is_some());
    Some(RolloutMeta {
        cwd: payload.get("cwd").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        is_subagent,
    })
}

// ---------- opencode ----------

fn is_opencode_binary(p: &str) -> bool {
    p.contains("/@opencode/cli/")
        || p.contains("/opencode/cli/bin/opencode")
        // Plain exec paths, including the `/opt/homebrew/bin/opencode` fallback
        // argv an upgrade leaves behind after unlinking the running binary.
        || p.ends_with("/opencode")
        || p.ends_with("/opencode.exe")
}

/// opencode resume: explicit `--session <id>` in argv is authoritative; a bare
/// TUI started through `ooc` carries no id (2.0.26 stopped logging
/// `session.id=`), so fall back to ranking the newest top-level rows of the
/// local session DB. `opencode run …` (one-shot prompt) and `opencode serve …`
/// (the stdio child that holds session state) are not conversation owners and
/// must not claim a session: argv[1] membership in the subcommand set is what
/// disqualifies a process — `opencode <dir>` is a TUI at a workspace path and
/// must still resolve.
///
/// The fallback ranks `(time_suspended IS NULL (open), directory == cwd,
/// time_updated)` and shares the per-tick claim set with codex so sibling tabs
/// never persist the same id. Without an argv id the mapping between tabs and
/// sessions is a best-effort guess — once replayed through `ooc --session
/// <id>`, later saves are exact.
fn opencode_resume(pid: c_int, cwd: &Path, ai_resume: &AiResumeConfig) -> Option<String> {
    let args = crate::macos::proc::argv(pid)?;
    if opencode_is_subcommand(&args) {
        return None;
    }
    if let Some(id) = opencode_session_id_from_args(&args) {
        return Some(opencode_resume_command(&id, ai_resume));
    }
    let rows = opencode_session_rows()?;
    let id = opencode_session_from_rows(&rows, cwd.to_str().unwrap_or(""))?;
    Some(opencode_resume_command(&id, ai_resume))
}

/// opencode subcommands. Anything else at argv[1] — a flag or a workspace
/// directory positional — means this process is the tab's interactive TUI.
const OPENCODE_SUBCOMMANDS: &[&str] = &[
    "run",
    "serve",
    "session",
    "service",
    "upgrade",
    "update",
    "uninstall",
    "acp",
    "api",
    "debug",
    "auth",
    "mcp",
    "plugin",
    "models",
    "stats",
    "mini",
    "reload",
    "pair",
];

fn opencode_is_subcommand(args: &[String]) -> bool {
    args.get(1).is_some_and(|arg| OPENCODE_SUBCOMMANDS.contains(&arg.as_str()))
}

fn opencode_session_id_from_args<S: AsRef<str>>(args: &[S]) -> Option<String> {
    for (idx, arg) in args.iter().enumerate() {
        let arg = arg.as_ref();
        if arg == "--session" || arg == "-s" {
            return args
                .get(idx + 1)
                .map(AsRef::as_ref)
                .filter(|id| !id.is_empty() && !id.starts_with('-'))
                .map(ToString::to_string);
        }
        if let Some(id) = arg.strip_prefix("--session=") {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn opencode_resume_command(session_id: &str, ai_resume: &AiResumeConfig) -> String {
    // Resume through the `ooc` wrapper so the tab reopens with the same
    // switchboard/provider policy as the original launch. Never `--continue`:
    // with multiple tabs it would collapse every one onto the newest session.
    build_command("ooc", &ai_resume.opencode.flags, ["--session", session_id])
}

/// Top-level opencode sessions, newest first:
/// `(id, directory, time_updated, open)`.
///
/// opencode keeps no PID-keyed session pointer; the sqlite store is the only
/// record. Read it out-of-process via `/usr/bin/sqlite3 -readonly -json` so the
/// terminal never links sqlite or writes to the user's session DB — and so a
/// `|` inside `directory` cannot corrupt the parse.
fn opencode_session_rows() -> Option<Vec<(String, String, i64, bool)>> {
    let mut db = home::home_dir()?;
    db.push(".local/share/opencode/opencode.db");
    let output = std::process::Command::new("/usr/bin/sqlite3")
        .args([
            "-readonly",
            "-json",
            db.to_str()?,
            "SELECT id, directory, time_updated, (time_suspended IS NULL) AS open \
             FROM session_v2 WHERE parent_id IS NULL \
             ORDER BY time_updated DESC LIMIT 32",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let values: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let mut rows = Vec::new();
    for row in values.as_array()? {
        let id = row.get("id")?.as_str()?;
        if id.is_empty() {
            continue;
        }
        let directory = row.get("directory").and_then(|d| d.as_str()).unwrap_or("");
        let time_updated = row.get("time_updated").and_then(|t| t.as_i64()).unwrap_or(0);
        let open = match row.get("open") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(v) => v.as_i64().unwrap_or(1) != 0,
            None => true,
        };
        rows.push((id.to_string(), directory.to_string(), time_updated, open));
    }
    Some(rows)
}

/// Testable inner ranking over `(id, directory, time_updated, open)` rows.
/// Rank is `(open, directory == cwd, time_updated)`, highest wins;
/// already-claimed ids are skipped so sibling tabs land on distinct sessions.
/// (`ooc` launches every session under one directory, so the cwd component is
/// often inert — the residual tab↔session shuffle is documented in FORK.md.)
fn opencode_session_from_rows(
    rows: &[(String, String, i64, bool)],
    cwd: &str,
) -> Option<String> {
    let claimed: HashSet<String> = claimed_ids().lock().map(|g| g.clone()).unwrap_or_default();
    let mut best: Option<((bool, bool, i64), String)> = None;
    for (id, directory, time_updated, open) in rows {
        if claimed.contains(id) {
            continue;
        }
        let rank = (*open, directory == cwd, *time_updated);
        if best.as_ref().is_none_or(|(best_rank, _)| *best_rank < rank) {
            best = Some((rank, id.clone()));
        }
    }
    let id = best.map(|(_, id)| id)?;
    if let Ok(mut g) = claimed_ids().lock() {
        g.insert(id.clone());
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_claude_binary() {
        assert!(is_claude_binary("/Users/x/.local/share/claude/versions/2.1.144"));
        assert!(is_claude_binary("/opt/claude/versions/2.0.0"));
        assert!(is_claude_binary("/Users/x/.local/bin/claude"));
        assert!(!is_claude_binary("/usr/bin/zsh"));
        assert!(!is_claude_binary("/Users/x/bin/claude-wrapper"));
    }

    #[test]
    fn claude_survives_version_prune() {
        // Long-idle claude tabs run a version binary the auto-updater later
        // deletes; their saved launch path is the `bin/claude` symlink.
        let tmp = tempfile::TempDir::new().unwrap();
        let versions = tmp.path().join("share/claude/versions");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let version = versions.join("2.1.302");
        let launcher = bin.join("claude");
        std::os::unix::fs::symlink(&version, &launcher).unwrap();
        let mut child =
            crate::macos::proc::tests::spawn_test_executable_at(&version, &launcher, None);
        std::fs::remove_file(&version).unwrap();

        let path = crate::macos::proc::pid_path(child.id() as c_int);
        // Always reap our fixture before asserting, including the regression case.
        child.kill().unwrap();
        child.wait().unwrap();
        let path = path.expect("pruned binary keeps its saved launch path");
        assert!(is_claude_binary(path.to_str().unwrap()), "unrecognized: {}", path.display());
    }

    #[test]
    fn matches_copilot_binary() {
        assert!(is_copilot_binary(
            "/opt/homebrew/lib/node_modules/@github/copilot/node_modules/@github/copilot-darwin-arm64/copilot"
        ));
        assert!(is_copilot_binary("/x/@github/copilot/foo"));
        assert!(!is_copilot_binary("/usr/bin/zsh"));
    }

    #[test]
    fn matches_codex_binary() {
        let codex_path = "/Users/x/.config/yarn/global/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/codex/codex";
        let codexpilot_path = "/opt/homebrew/lib/node_modules/codexpilot/node_modules/codexpilot-darwin-arm64/vendor/aarch64-apple-darwin/codex/codex";

        assert_eq!(codex_program_for_binary(codex_path), Some("codex"));
        assert_eq!(codex_program_for_binary(codexpilot_path), Some("codexpilot"));
        assert!(codex_program_for_binary(codex_path).is_some());
        assert!(codex_program_for_binary(codexpilot_path).is_some());
        assert!(codex_program_for_binary("/x/@openai/codex/foo").is_some());
        assert!(codex_program_for_binary("/usr/bin/zsh").is_none());
    }

    #[test]
    fn resume_commands_use_configured_toml_flags() {
        let config = test_ai_resume_config();
        assert_eq!(
            claude_resume_command("claude-session", None, &config),
            "claude --toml-claude-flag --resume claude-session"
        );
        assert_eq!(
            copilot_resume_command("copilot-session", &config),
            "copilot --toml-copilot-flag --mouse=on --resume=copilot-session"
        );
        assert_eq!(
            codex_resume_command("codex", "codex-session", &config),
            "codex --toml-codex-flag resume codex-session"
        );
        assert_eq!(
            codex_resume_command("codexpilot", "codex-session", &config),
            "codexpilot --toml-codex-flag resume codex-session"
        );
        assert_eq!(
            opencode_resume_command("ses_abc", &config),
            "ooc --toml-opencode-flag --session ses_abc"
        );
    }

    #[test]
    fn cglm_resume_preserves_provider_and_flash_across_save_load() {
        let config = test_ai_resume_config();
        for (model, prefix) in [("sonnet", "cglm"), ("haiku", "cglm --flash")] {
            // Match ~/.zshrc's cglm launch, including an unrelated setting.
            let args = [
                "claude",
                "--model",
                model,
                "--effort",
                "max",
                "--settings",
                r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.z.ai/api/anthropic","ENABLE_TOOL_SEARCH":"0"}}"#,
            ]
            .map(String::from);
            let command = claude_resume_from_args("glm-session", None, &args, &config).unwrap();
            assert_eq!(command, format!("{prefix} --toml-claude-flag --resume glm-session"));
            let serialized = serde_json::to_string(&command).unwrap();
            let saved: String = serde_json::from_str(&serialized).unwrap();
            let mut updated = config.clone();
            updated.claude.flags = vec!["--new-policy".into()];
            assert_eq!(
                normalize_saved_resume_command(&saved, &updated),
                Some(format!("{prefix} --new-policy --resume glm-session"))
            );
        }
    }

    #[test]
    fn claude_resume_keeps_subscription_without_glm_endpoint() {
        let config = test_ai_resume_config();
        for args in [
            vec!["claude", "--model", "haiku"],
            vec!["claude", "--settings", r#"{"env":{"ENABLE_TOOL_SEARCH":"0"}}"#],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert_eq!(
                claude_resume_from_args("subscription-session", None, &args, &config).as_deref(),
                Some("claude --toml-claude-flag --resume subscription-session")
            );
        }
    }

    #[test]
    fn cglm_resume_handles_equals_options_and_last_model_override() {
        let args = [
            "claude",
            "--model=haiku",
            "--settings={\"env\":{\"ANTHROPIC_BASE_URL\":\"https://api.z.ai/api/anthropic/\"}}",
            "--model=sonnet",
        ]
        .map(String::from);
        assert_eq!(
            claude_resume_from_args("glm-session", None, &args, &AiResumeConfig::default()).as_deref(),
            Some("cglm --resume glm-session")
        );
    }

    #[test]
    fn claude_resume_keeps_unknown_provider_without_subscription_fallback() {
        let args = [
            "claude",
            "--settings",
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example/api/anthropic","SECRET_TOKEN":"never-persist"}}"#,
        ]
        .map(String::from);
        let command = claude_resume_from_args("session", None, &args, &AiResumeConfig::default()).unwrap();
        // The endpoint survives; the bare subscription launcher must not
        // replace it (that would burn paid credits on a foreign-router chat)…
        assert_eq!(
            command,
            "claude --settings '{\"env\":{\"ANTHROPIC_BASE_URL\":\"https://other.example/api/anthropic\"}}' --resume session"
        );
        // …and no other env from the live --settings (tokens, tuning) leaks.
        assert!(!command.contains("SECRET_TOKEN"));
        // Round-trips through save/load normalization unchanged.
        assert_eq!(
            normalize_saved_resume_command(&command, &AiResumeConfig::default()).as_deref(),
            Some(command.as_str())
        );
    }

    #[test]
    fn claude_resume_rejects_non_http_provider_urls() {
        for url in ["ftp://other.example/api", "not-a-url", "file:///tmp/x"] {
            let settings = format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{url}"}}}}"#);
            let args = ["claude", "--settings", &settings].map(String::from);
            assert!(
                claude_resume_from_args("session", None, &args, &AiResumeConfig::default()).is_none(),
                "url {url} must not produce a resume command"
            );
        }
    }

    #[test]
    fn claude_resume_routes_endpoints_through_originating_wrappers() {
        // Each zshrc wrapper owns its auth env; the replay must go through the
        // wrapper (never a bare claude that would drop tokens/model maps).
        for (endpoint, wrapper) in [
            ("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic", "cqwn"),
            ("https://token-plan-sgp.xiaomimimo.com/anthropic", "cmo"),
            ("http://127.0.0.1:8081", "cfu"),
            ("https://openrouter.ai/api", "cor"),
            ("http://127.0.0.1:8080", "coc"),
        ] {
            let settings = format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{endpoint}","ANTHROPIC_AUTH_TOKEN":"never-persist"}}}}"#);
            let args = ["claude", "--settings", &settings].map(String::from);
            let command =
                claude_resume_from_args("session", None, &args, &AiResumeConfig::default())
                    .unwrap();
            assert_eq!(command, format!("{wrapper} --resume session"), "{endpoint}");
            assert!(!command.contains("never-persist"), "{endpoint}");
        }
    }

    #[test]
    fn crt_resume_pins_account_store_and_round_trips() {
        let config = AiResumeConfig::default();
        let settings = r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8082"}}"#;
        let args = ["claude", "--settings", settings].map(String::from);
        let store = "/Users/x/.claude-accounts/acct3";
        let command = claude_resume_from_args("session", Some(store), &args, &config).unwrap();
        assert_eq!(command, "crt --account acct3 --resume session");
        assert_eq!(
            normalize_saved_resume_command(&command, &config).as_deref(),
            Some(command.as_str())
        );
        // Stock-store sessions pin `default` so crt cannot rotate onto another
        // account's login mid-resume.
        let command = claude_resume_from_args("session", None, &args, &config).unwrap();
        assert_eq!(command, "crt --account default --resume session");
    }

    #[test]
    fn claude_account_store_binding_survives_save_load() {
        let config = AiResumeConfig::default();
        let args = ["claude", "--model", "sonnet"].map(String::from);
        let store = "/Users/x/.claude-accounts/acct2";
        let command =
            claude_resume_from_args("subscription-session", Some(store), &args, &config).unwrap();
        assert_eq!(
            command,
            "CLAUDE_CONFIG_DIR=/Users/x/.claude-accounts/acct2 claude --resume subscription-session"
        );
        assert_eq!(
            normalize_saved_resume_command(&command, &config).as_deref(),
            Some(command.as_str())
        );
        // Non-account wrappers inherit the store the same way.
        let settings = r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.z.ai/api/anthropic"}}"#;
        let args = ["claude", "--settings", settings].map(String::from);
        let command =
            claude_resume_from_args("glm-session", Some(store), &args, &config).unwrap();
        assert_eq!(
            command,
            "CLAUDE_CONFIG_DIR=/Users/x/.claude-accounts/acct2 cglm --resume glm-session"
        );
        assert_eq!(
            normalize_saved_resume_command(&command, &config).as_deref(),
            Some(command.as_str())
        );
    }

    #[test]
    fn wrapper_resume_round_trips_flash_and_flags() {
        let mut config = AiResumeConfig::default();
        config.claude.flags = vec!["--toml-claude-flag".into()];
        let settings = r#"{"env":{"ANTHROPIC_BASE_URL":"https://token-plan-sgp.xiaomimimo.com/anthropic"}}"#;
        let args = ["claude", "--model", "haiku", "--settings", settings].map(String::from);
        let command = claude_resume_from_args("mimo-session", None, &args, &config).unwrap();
        assert_eq!(command, "cmo --flash --toml-claude-flag --resume mimo-session");
        let mut updated = config.clone();
        updated.claude.flags = vec!["--new-policy".into()];
        assert_eq!(
            normalize_saved_resume_command(&command, &updated).as_deref(),
            Some("cmo --flash --new-policy --resume mimo-session")
        );
    }

    #[test]
    fn parse_codex_daemon_pid_validates_identity() {
        // Fresh record, no identity block: pid accepted.
        assert_eq!(parse_codex_daemon_pid(r#"{"pid":3135}"#), Some(3135));
        // Identity mismatch (startSeconds cannot match any live process start
        // of a PID that does not exist): rejected, stale file never trusted.
        let stale = r#"{"pid":4000000000,"processIdentity":{"startSeconds":1}}"#;
        assert_eq!(parse_codex_daemon_pid(stale), None);
        let out_of_range = r#"{"pid":4000000000,"processIdentity":{"startSeconds":1791661164}}"#;
        assert_eq!(parse_codex_daemon_pid(out_of_range), None);
        assert_eq!(parse_codex_daemon_pid("not json"), None);
        assert_eq!(parse_codex_daemon_pid(r#"{"pid":-1}"#), None);
    }

    #[test]
    fn codex_claimed_rollouts_not_returned_twice() {
        // Daemon-layout shared fd pool: the claim set distributes distinct
        // rollouts across sibling tabs instead of replaying one id twice.
        // TempDir removes the directory on drop, so a failing assert or an
        // unwrap panic below does not leak a pid-keyed directory in $TMPDIR.
        let tmp = tempfile::TempDir::new().unwrap();
        let date_dir = tmp.path().join(".codex/sessions/2026/10/11");
        std::fs::create_dir_all(&date_dir).unwrap();
        let write_rollout = |uuid: &str| {
            let path = date_dir.join(format!("rollout-2026-10-11T00-00-00-{uuid}.jsonl"));
            std::fs::write(&path, r#"{"payload":{"cwd":"/work","source":"cli"}}"#).unwrap();
            // Pin equal mtimes: rollout_mtime is second-granular, so two
            // immediate writes could straddle a second boundary.
            let status = std::process::Command::new("/usr/bin/touch")
                .args(["-t", "202601010000.00", path.to_str().unwrap()])
                .status()
                .unwrap();
            assert!(status.success());
            path
        };
        let first = write_rollout("aaaaaaaa-0000-0000-0000-000000000001");
        let second = write_rollout("bbbbbbbb-0000-0000-0000-000000000002");
        let paths = vec![first, second];
        begin_save_tick();
        // Equal ranks: the first-listed rollout wins, then the claim advances.
        assert_eq!(
            codex_session_from_open_paths(&paths, "/work").as_deref(),
            Some("aaaaaaaa-0000-0000-0000-000000000001")
        );
        assert_eq!(
            codex_session_from_open_paths(&paths, "/work").as_deref(),
            Some("bbbbbbbb-0000-0000-0000-000000000002")
        );
        assert_eq!(codex_session_from_open_paths(&paths, "/work"), None);
    }

    #[test]
    fn resume_commands_use_configured_flags() {
        let mut config = test_ai_resume_config();
        config.codex.flags = vec!["--sandbox".into(), "workspace-write".into()];
        assert_eq!(
            codex_resume_command("codex", "codex-session", &config),
            "codex --sandbox workspace-write resume codex-session"
        );
    }

    #[test]
    fn resume_command_quotes_configured_flags_for_shell() {
        let mut config = test_ai_resume_config();
        config.codex.flags = vec!["--config".into(), "model=\"gpt-5 codex\"".into()];
        assert_eq!(
            codex_resume_command("codex", "codex-session", &config),
            "codex --config 'model=\"gpt-5 codex\"' resume codex-session"
        );
    }

    #[test]
    fn codex_resume_arg_extracts_explicit_session() {
        let args = vec![
            "codex".to_string(),
            "--existing-flag".to_string(),
            "resume".to_string(),
            "019e53d4-6f52-7ad1-a5c4-171284c2f248".to_string(),
        ];
        assert_eq!(
            codex_resume_arg_from(&args).as_deref(),
            Some("019e53d4-6f52-7ad1-a5c4-171284c2f248")
        );
    }

    #[test]
    fn codex_resume_arg_rejects_last_fallback() {
        let args = vec!["codex".to_string(), "resume".to_string(), "--last".to_string()];
        assert!(codex_resume_arg_from(&args).is_none());
    }

    #[test]
    fn matches_opencode_binary() {
        assert!(is_opencode_binary("/opt/homebrew/lib/node_modules/@opencode/cli/bin/opencode.exe"));
        assert!(is_opencode_binary("/x/@opencode/cli/bin/opencode"));
        assert!(is_opencode_binary("/opt/homebrew/bin/opencode"));
        assert!(!is_opencode_binary("/usr/bin/zsh"));
    }

    #[test]
    fn opencode_resume_arg_extracts_explicit_session() {
        for args in [
            vec!["opencode", "--session", "ses_abc"],
            vec!["opencode", "--standalone", "--auto", "--session=ses_abc"],
            vec!["opencode", "-s", "ses_abc"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert_eq!(opencode_session_id_from_args(&args).as_deref(), Some("ses_abc"));
        }
    }

    #[test]
    fn opencode_resume_arg_rejects_missing_or_flag_values() {
        for args in [
            vec!["opencode", "--session"],
            vec!["opencode", "--session", "--auto"],
            vec!["opencode", "--standalone", "--auto"],
            vec!["opencode", "--session="],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert!(opencode_session_id_from_args(&args).is_none(), "{args:?}");
        }
    }

    #[test]
    fn opencode_subcommands_are_not_resume_targets() {
        let tui = ["opencode", "--standalone", "--auto"].map(String::from);
        assert!(!opencode_is_subcommand(&tui));
        // `opencode <dir>` is a TUI at a workspace path, not a subcommand.
        for args in [vec!["opencode", "/Users/x/Projects"], vec!["opencode"]] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert!(!opencode_is_subcommand(&args), "{args:?}");
        }
        for args in [
            vec!["opencode", "run", "--standalone", "--auto", "prompt"],
            vec!["opencode", "serve", "--stdio", "--port", "0"],
            vec!["opencode", "session", "list"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert!(opencode_is_subcommand(&args), "{args:?}");
        }
    }

    #[test]
    fn opencode_session_ranking_prefers_open_cwd_then_recency() {
        let rows = vec![
            ("ses_closed_newest".to_string(), "/work".to_string(), 400, false),
            ("ses_open_old_cwd".to_string(), "/work".to_string(), 100, true),
            ("ses_open_elsewhere".to_string(), "/other".to_string(), 300, true),
            ("ses_open_newest_cwd".to_string(), "/work".to_string(), 200, true),
        ];
        assert_eq!(
            opencode_session_from_rows(&rows, "/work").as_deref(),
            Some("ses_open_newest_cwd")
        );
    }

    #[test]
    fn opencode_claimed_sessions_not_returned_twice() {
        begin_save_tick();
        let rows = vec![
            ("ses_first".to_string(), "/work".to_string(), 300, true),
            ("ses_second".to_string(), "/work".to_string(), 200, true),
        ];
        assert_eq!(opencode_session_from_rows(&rows, "/work").as_deref(), Some("ses_first"));
        assert_eq!(opencode_session_from_rows(&rows, "/work").as_deref(), Some("ses_second"));
        assert_eq!(opencode_session_from_rows(&rows, "/work").as_deref(), None);
    }

    #[test]
    fn opencode_resume_round_trips_session_flag() {
        let config = test_ai_resume_config();
        assert_eq!(
            opencode_resume_command("ses_abc", &config),
            "ooc --toml-opencode-flag --session ses_abc"
        );
        assert_eq!(
            normalize_saved_resume_command("ooc --session ses_abc", &config),
            Some("ooc --toml-opencode-flag --session ses_abc".to_string())
        );
        // `--continue` is multi-tab collapse fuel, same as `codex resume --last`.
        assert_eq!(normalize_saved_resume_command("ooc --continue", &config), None);
    }

    #[test]
    fn normalize_saved_resume_command_upgrades_old_flags() {
        let config = test_ai_resume_config();
        assert_eq!(
            normalize_saved_resume_command("claude --resume claude-session", &config).as_deref(),
            Some("claude --toml-claude-flag --resume claude-session")
        );
        assert_eq!(
            normalize_saved_resume_command("codex --existing-flag resume codex-session", &config,)
                .as_deref(),
            Some("codex --toml-codex-flag resume codex-session")
        );
        assert_eq!(
            normalize_saved_resume_command(
                "codexpilot --existing-flag resume codex-session",
                &config,
            )
            .as_deref(),
            Some("codexpilot --toml-codex-flag resume codex-session")
        );
        assert_eq!(
            normalize_saved_resume_command("copilot --resume=copilot-session", &config).as_deref(),
            Some("copilot --toml-copilot-flag --mouse=on --resume=copilot-session")
        );
    }

    #[test]
    fn normalize_saved_resume_command_rejects_codex_last() {
        let config = test_ai_resume_config();
        assert!(
            normalize_saved_resume_command("codex --existing-flag resume --last", &config,)
                .is_none()
        );
        assert!(
            normalize_saved_resume_command("codexpilot --existing-flag resume --last", &config,)
                .is_none()
        );
    }

    #[test]
    fn copilot_scan_resets_on_pid_reuse_with_new_log() {
        // spk-961c23c40386: the PID-keyed scan cache must not carry a byte
        // offset across to a different log file. macOS reuses PIDs; the new
        // copilot's log has a different inode, and if it happens to be at
        // least as long as the stale offset the incremental scan would start
        // mid-file and miss the session marker.
        // TempDir removes the directory on drop, so a failing assert or an
        // unwrap panic below does not leak a pid-keyed directory in $TMPDIR.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        // A PID no real copilot can hold, so parallel tests sharing the
        // process-lifetime cache cannot collide with this entry.
        let pid: c_int = 2_000_000_007;
        let log_a = dir.join("process-aaa-2000000007.log");
        let log_b = dir.join("process-bbb-2000000007.log");
        std::fs::write(&log_a, "boot\nRegistering foreground session: uuid-aaa\nmore\n").unwrap();
        assert_eq!(copilot_scan_uuid(pid, &log_a).as_deref(), Some("uuid-aaa"));
        // Replacement log under the recycled PID: longer than the 52-byte
        // offset stored above, with its only marker before that offset.
        std::fs::write(
            &log_b,
            "Registering foreground session: uuid-bbb\nfiller-line-0123456789\n",
        )
        .unwrap();
        assert!(std::fs::metadata(&log_b).unwrap().len() >= 52);
        assert_eq!(copilot_scan_uuid(pid, &log_b).as_deref(), Some("uuid-bbb"));
    }

    #[test]
    fn copilot_resume_defaults_mouse_on_when_unspecified() {
        let config = AiResumeConfig::default();
        assert_eq!(
            copilot_resume_command("copilot-session", &config),
            "copilot --mouse=on --resume=copilot-session"
        );
    }

    #[test]
    fn copilot_resume_respects_explicit_mouse_flags() {
        let mut config = AiResumeConfig::default();
        config.copilot.flags = vec!["--mouse=off".into(), "--toml-copilot-flag".into()];
        assert_eq!(
            copilot_resume_command("copilot-session", &config),
            "copilot --mouse=off --toml-copilot-flag --resume=copilot-session"
        );

        config.copilot.flags = vec!["--no-mouse".into()];
        assert_eq!(
            copilot_resume_command("copilot-session", &config),
            "copilot --no-mouse --resume=copilot-session"
        );

        config.copilot.flags = vec!["--mouse".into(), "off".into()];
        assert_eq!(
            copilot_resume_command("copilot-session", &config),
            "copilot --mouse off --resume=copilot-session"
        );
    }

    fn test_ai_resume_config() -> AiResumeConfig {
        let mut config = AiResumeConfig::default();
        config.claude.flags = vec!["--toml-claude-flag".into()];
        config.codex.flags = vec!["--toml-codex-flag".into()];
        config.copilot.flags = vec!["--toml-copilot-flag".into()];
        config.opencode.flags = vec!["--toml-opencode-flag".into()];
        config
    }

    // ── rollout_uuid_from_path ────────────────────────────────────────────

    #[test]
    fn rollout_uuid_parses_session_path() {
        let p = std::path::PathBuf::from(
            "/Users/x/.codex/sessions/2026/07/25/rollout-2026-07-25T16-11-06-019f9b1e-1076-7d10-941b-136a6d4291ad.jsonl",
        );
        assert_eq!(
            rollout_uuid_from_path(&p).as_deref(),
            Some("019f9b1e-1076-7d10-941b-136a6d4291ad")
        );
    }

    #[test]
    fn rollout_uuid_rejects_non_session_paths() {
        // Right name, wrong tree — an ordinary open file must never be
        // mistaken for a session.
        let outside = std::path::PathBuf::from(
            "/tmp/rollout-2026-07-25T16-11-06-019f9b1e-1076-7d10-941b-136a6d4291ad.jsonl",
        );
        assert!(rollout_uuid_from_path(&outside).is_none());

        // Right tree, not a rollout.
        let other = std::path::PathBuf::from("/Users/x/.codex/sessions/2026/07/25/notes.jsonl");
        assert!(rollout_uuid_from_path(&other).is_none());

        // Truncated body with no uuid.
        let short = std::path::PathBuf::from(
            "/Users/x/.codex/sessions/2026/07/25/rollout-2026-07-25T16-11-06.jsonl",
        );
        assert!(rollout_uuid_from_path(&short).is_none());
    }

    // ── codex_session_from_open_paths ─────────────────────────────────────

    /// Write a rollout fixture and return its path.
    ///
    /// `source` is the raw JSON for `payload.source` — `"\"cli\""` for a user
    /// session, `r#"{"subagent": {...}}"#` for a subagent thread.
    fn make_rollout(
        tmp: &tempfile::TempDir,
        date_sub: &str,
        ts: &str,
        uuid: &str,
        cwd: &str,
        is_subagent: bool,
    ) -> std::path::PathBuf {
        // Mirror the real layout so the `/.codex/sessions/` guard is exercised.
        let dir = tmp.path().join(".codex").join("sessions").join(date_sub);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{}-{}.jsonl", ts, uuid));
        let source = if is_subagent {
            r#"{"subagent": {"thread_spawn": {"parent_thread_id": "019f9b1f-687f-7ae2"}}}"#
        } else {
            r#""cli""#
        };
        let line = format!(
            r#"{{"type":"session_meta","payload":{{"cwd":"{}","source":{}}}}}"#,
            cwd, source
        );
        std::fs::write(&path, line).unwrap();
        path
    }

    /// Stamp a file's mtime, so ranking tests don't depend on write order.
    fn set_mtime(path: &Path, unix_secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs)).unwrap();
    }

    #[test]
    fn open_rollout_resolves_regardless_of_age() {
        // The regression this whole path exists for: a resumed conversation's
        // rollout is dated days before the process, which the old start-time
        // matcher rejected outright. Held open, it must still resolve.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let old = make_rollout(&tmp, "2026/07/11", "2026-07-11T16-36-37", "uuid-old", cwd, false);
        begin_save_tick();
        assert_eq!(
            codex_session_from_open_paths(&[old], cwd).as_deref(),
            Some("uuid-old"),
            "an old-but-open rollout should resolve"
        );
    }

    #[test]
    fn subagent_rollouts_are_ignored() {
        // Shape taken from a live busy codex: one `cli` session plus a pile of
        // subagent threads, all held open by the same process.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let user = make_rollout(&tmp, "2026/07/25", "2026-07-25T16-12-34", "uuid-user", cwd, false);
        let sub_a =
            make_rollout(&tmp, "2026/07/27", "2026-07-27T04-20-32", "uuid-sub-a", cwd, true);
        let sub_b =
            make_rollout(&tmp, "2026/07/27", "2026-07-27T11-14-51", "uuid-sub-b", cwd, true);
        // Subagents are NEWER than the user session — mtime must not save them.
        set_mtime(&user, 1_000);
        set_mtime(&sub_a, 9_000);
        set_mtime(&sub_b, 9_999);

        begin_save_tick();
        assert_eq!(
            codex_session_from_open_paths(&[sub_a, user, sub_b], cwd).as_deref(),
            Some("uuid-user"),
            "only the user-initiated session should be resumable"
        );
    }

    #[test]
    fn cwd_match_outranks_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let elsewhere =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-00-00", "uuid-other", "/other", false);
        let here = make_rollout(&tmp, "2026/07/25", "2026-07-25T10-01-00", "uuid-here", cwd, false);
        set_mtime(&here, 1_000);
        set_mtime(&elsewhere, 9_000);

        begin_save_tick();
        assert_eq!(
            codex_session_from_open_paths(&[elsewhere, here], cwd).as_deref(),
            Some("uuid-here"),
            "the session matching the tab's cwd should win despite older mtime"
        );
    }

    #[test]
    fn cwd_mismatch_is_demoted_not_rejected() {
        // Launching codex after a `cd` leaves the tab cwd differing from the
        // recorded session cwd. The open fd still proves ownership.
        let tmp = tempfile::TempDir::new().unwrap();
        let other =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-00-00", "uuid-other", "/other", false);
        begin_save_tick();
        assert_eq!(
            codex_session_from_open_paths(&[other], "/Users/x/project").as_deref(),
            Some("uuid-other"),
            "sole candidate should resolve even when cwd differs"
        );
    }

    #[test]
    fn newest_wins_among_equal_cwd() {
        // After an in-process `/new`, codex may still hold the prior
        // conversation open; the active one is the more recently written.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let older =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-00-00", "uuid-older", cwd, false);
        let newer =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-01-00", "uuid-newer", cwd, false);
        set_mtime(&older, 1_000);
        set_mtime(&newer, 2_000);

        begin_save_tick();
        assert_eq!(
            codex_session_from_open_paths(&[older, newer], cwd).as_deref(),
            Some("uuid-newer")
        );
    }

    #[test]
    fn claimed_uuid_is_not_returned_twice() {
        // Two tabs in one save tick must never persist the same session id.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let first =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-01-00", "uuid-first", cwd, false);
        let second =
            make_rollout(&tmp, "2026/07/25", "2026-07-25T10-02-00", "uuid-second", cwd, false);
        set_mtime(&first, 2_000);
        set_mtime(&second, 1_000);

        begin_save_tick();
        let paths = vec![first, second];
        let a = codex_session_from_open_paths(&paths, cwd);
        // No begin_save_tick between tabs — same save tick.
        let b = codex_session_from_open_paths(&paths, cwd);
        assert_eq!(a.as_deref(), Some("uuid-first"));
        assert_eq!(b.as_deref(), Some("uuid-second"), "second tab must not reuse the first id");
    }

    #[test]
    fn codex_resume_survives_executable_removal() {
        // A real child holds a rollout open under a Codex vendor path. Removing
        // its executable reproduces an npm upgrade without launching an AI CLI.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let rollout =
            make_rollout(&tmp, "2026/07/11", "2026-07-11T16-36-37", "uuid-upgraded", cwd, false);
        let executable = tmp.path().join("@openai/codex/bin/codex");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        let mut child =
            crate::macos::proc::tests::spawn_test_executable(&executable, Some(&rollout));
        std::fs::remove_file(&executable).unwrap();

        begin_save_tick();
        let command = resume_command_for(
            std::process::id() as c_int,
            Path::new(cwd),
            &test_ai_resume_config(),
        );
        // Always reap our fixture before asserting, including the regression case.
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(command.as_deref(), Some("codex --toml-codex-flag resume uuid-upgraded"));
    }

    #[test]
    fn no_open_rollouts_returns_none() {
        begin_save_tick();
        let unrelated = std::path::PathBuf::from("/usr/lib/libSystem.dylib");
        assert!(codex_session_from_open_paths(&[unrelated], "/any/cwd").is_none());
        assert!(codex_session_from_open_paths(&[], "/any/cwd").is_none());
    }
}
