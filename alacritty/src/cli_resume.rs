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
//! | claude               | `~/.claude/sessions/<pid>.json` → `sessionId`  | `claude <flags> --resume <id>`        |
//! | cglm (Claude + GLM)   | Same marker, z.ai endpoint in argv settings    | `cglm [--flash] <flags> --resume <id>` |
//! | copilot              | `~/.copilot/logs/process-<ts>-<pid>.log` last  | `copilot <flags> --resume=<id>`       |
//! |                      | "Registering foreground session: <uuid>" entry |                                       |
//! | codex / codexpilot   | argv `resume <uuid>`, else the rollout         | `<program> <flags> resume <id>`       |
//! |                      | `.jsonl` the process holds OPEN                |                                       |
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

/// Codex session UUIDs claimed by sibling windows during the current
/// save-tick. Cleared by `begin_save_tick()` from the save worker before
/// each iteration over windows. Ensures that two codex tabs whose PIDs
/// started in the same second don't both resolve to the same earliest
/// rollout — the first window to call grabs it, the second gets the
/// next earliest in its window.
static CLAIMED_CODEX_UUIDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn claimed_set() -> &'static Mutex<HashSet<String>> {
    CLAIMED_CODEX_UUIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Clear the per-save-tick claim set. Call once at the start of each
/// `save_session` iteration over windows.
pub fn begin_save_tick() {
    if let Ok(mut g) = claimed_set().lock() {
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

fn is_claude_binary(p: &str) -> bool {
    p.contains("/.claude/versions/") || p.contains("/claude/versions/")
}

/// claude maintains `~/.claude/sessions/<pid>.json` with `sessionId` while
/// the process is alive. We read it directly — minimal-parse JSON.
fn claude_resume(pid: c_int, ai_resume: &AiResumeConfig) -> Option<String> {
    let mut p = home::home_dir()?;
    p.push(".claude");
    p.push("sessions");
    p.push(format!("{}.json", pid));
    let raw = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let session_id = v.get("sessionId")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    let args = crate::macos::proc::argv(pid)?;
    claude_resume_from_args(session_id, &args, ai_resume)
}

fn claude_resume_command(session_id: &str, ai_resume: &AiResumeConfig) -> String {
    build_command("claude", &ai_resume.claude.flags, ["--resume", session_id])
}

/// The cglm shell function execs the same Claude binary, but includes its
/// provider endpoint in inline --settings. Preserve that launcher instead of
/// silently resuming with subscription credentials. Never persist auth tokens.
fn claude_resume_from_args(
    session_id: &str,
    args: &[String],
    ai_resume: &AiResumeConfig,
) -> Option<String> {
    let mut endpoint = None;
    let mut flash = false;
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        let settings = if arg == "--settings" {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("--settings=")
        };
        if let Some(settings) = settings {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(settings) {
                if let Some(url) = value.get("env").and_then(|env| env.get("ANTHROPIC_BASE_URL")) {
                    endpoint = Some(url.as_str()?.to_owned());
                }
            }
            continue;
        }
        let model = if arg == "--model" {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("--model=")
        };
        if let Some(model) = model {
            flash = model == "haiku";
        }
    }

    match endpoint.as_deref().map(|url| url.trim_end_matches('/')) {
        Some("https://api.z.ai/api/anthropic") => {
            Some(cglm_resume_command(session_id, flash, ai_resume))
        },
        None | Some("") | Some("https://api.anthropic.com") => {
            Some(claude_resume_command(session_id, ai_resume))
        },
        // An unrecognized provider must not fall back to paid Claude credits.
        Some(_) => None,
    }
}

fn cglm_resume_command(session_id: &str, flash: bool, ai_resume: &AiResumeConfig) -> String {
    // cglm consumes --flash only at the start, before forwarding Claude flags.
    let tail = flash
        .then_some("--flash")
        .into_iter()
        .chain(ai_resume.claude.flags.iter().map(String::as_str))
        .chain(["--resume", session_id]);
    build_command("cglm", &[], tail)
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
/// this alacritty run.
struct CopilotScanState {
    offset: u64,
    last_uuid: Option<String>,
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
    let state = states.entry(pid).or_insert(CopilotScanState { offset: 0, last_uuid: None });

    let mut f = File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
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
    let args: Vec<&str> = command.split_whitespace().collect();
    let program = args.first()?;
    if *program == "cglm" {
        return claude_resume_id_from_args(&args)
            .map(|id| cglm_resume_command(&id, args.get(1) == Some(&"--flash"), ai_resume));
    }
    if program.ends_with("claude") {
        return claude_resume_id_from_args(&args).map(|id| claude_resume_command(&id, ai_resume));
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
    let open_paths = crate::macos::proc::open_file_paths(pid);
    codex_session_from_open_paths(&open_paths, cwd.to_str().unwrap_or(""))
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
fn codex_session_from_open_paths(open_paths: &[std::path::PathBuf], cwd: &str) -> Option<String> {
    // UUIDs already taken by a sibling window in this save-tick are skipped.
    // Distinct codex processes hold distinct fds so collisions shouldn't
    // happen, but claiming keeps two tabs from ever persisting the same id.
    let claimed: HashSet<String> = claimed_set().lock().map(|g| g.clone()).unwrap_or_default();

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
    if let Ok(mut g) = claimed_set().lock() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_claude_binary() {
        assert!(is_claude_binary("/Users/x/.local/share/claude/versions/2.1.144"));
        assert!(is_claude_binary("/opt/claude/versions/2.0.0"));
        assert!(!is_claude_binary("/usr/bin/zsh"));
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
            claude_resume_command("claude-session", &config),
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
            let command = claude_resume_from_args("glm-session", &args, &config).unwrap();
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
                claude_resume_from_args("subscription-session", &args, &config).as_deref(),
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
            claude_resume_from_args("glm-session", &args, &AiResumeConfig::default()).as_deref(),
            Some("cglm --resume glm-session")
        );
    }

    #[test]
    fn claude_resume_does_not_replace_unknown_provider_with_subscription() {
        let args = [
            "claude",
            "--settings",
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example/api/anthropic"}}"#,
        ]
        .map(String::from);
        assert!(claude_resume_from_args("session", &args, &AiResumeConfig::default()).is_none());
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
        assert!(normalize_saved_resume_command("codex --existing-flag resume --last", &config,)
            .is_none());
        assert!(normalize_saved_resume_command(
            "codexpilot --existing-flag resume --last",
            &config,
        )
        .is_none());
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
