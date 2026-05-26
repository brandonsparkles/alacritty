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
//! | Tool    | Identifier source                                     | Resume command                   |
//! |---------|-------------------------------------------------------|----------------------------------|
//! | claude  | `~/.claude/sessions/<pid>.json` → `sessionId`         | `claude <flags> --resume <id>`   |
//! | copilot | `~/.copilot/logs/process-<ts>-<pid>.log` last         | `copilot <flags> --resume=<id>`  |
//! |         | "Registering foreground session: <uuid>" entry        |                                  |
//! | codex   | argv `resume <uuid>` or rollout metadata              | `codex <flags> resume <id>`      |
//!
//! Tools are matched by their resolved binary path (via `proc_pidpath`),
//! not `pbi_comm`, because `pbi_comm` is truncated to 15 chars and reflects
//! the symlink target's basename (e.g. claude shows as `"2.1.144"`).
//!
//! ## Codex caveat
//!
//! codex stores sessions as `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`
//! with no PID-keyed marker we can use to map a fresh running codex process
//! back to its specific session. If codex was started by our restore command,
//! argv contains `resume <uuid>` and wins; otherwise we join by process start
//! time and cwd. We deliberately do not persist `resume --last` because it
//! makes multiple restored tabs open the same most-recent conversation.

#![cfg(target_os = "macos")]

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::raw::c_int;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::config::ai_resume::AiResumeConfig;

/// Codex session UUIDs claimed by sibling windows during the current
/// save-tick. Cleared by `begin_save_tick()` from the event loop before
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
                } else if is_codex_binary(path_str) {
                    if let Some(cmd) = codex_resume(child, _shell_cwd, ai_resume) {
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
    Some(claude_resume_command(session_id, ai_resume))
}

fn claude_resume_command(session_id: &str, ai_resume: &AiResumeConfig) -> String {
    build_command("claude", &ai_resume.claude.flags, ["--resume", session_id])
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

    let f = File::open(&log).ok()?;
    let reader = BufReader::new(f);
    const MARKER: &str = "Registering foreground session: ";
    let mut last_uuid: Option<String> = None;
    for line in reader.lines().map_while(Result::ok) {
        if let Some(idx) = line.find(MARKER) {
            let tail = line[idx + MARKER.len()..].trim();
            if !tail.is_empty() {
                last_uuid = Some(tail.to_string());
            }
        }
    }
    let uuid = last_uuid?;
    Some(copilot_resume_command(&uuid, ai_resume))
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

fn is_codex_binary(p: &str) -> bool {
    p.contains("@openai/codex") || p.contains("/codex-darwin-")
}

/// codex doesn't write PID-keyed session metadata anywhere, but each
/// codex process creates exactly one rollout `.jsonl` file shortly after
/// it starts:
///
///   `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<iso-ts>-<session-uuid>.jsonl`
///
/// We can recover the per-PID session ID by joining `pbi_start_tvsec`
/// against the filename's timestamp:
///
///  1. Read the codex process's start time.
///  2. Walk the rollouts in today's and yesterday's date dirs (handles a
///     codex session that spans midnight).
///  3. Open each rollout's first record (a `session_meta` line) and keep
///     only those whose `payload.cwd` matches the tab's cwd AND whose
///     `payload.source` is *not* a `subagent` (we want user-initiated
///     sessions, not codex's internal subagent rollouts).
///  4. Score each candidate by `|filename_timestamp - process_start|` and
///     pick the smallest delta within a reasonable window.
///
/// Returns `None` when no exact session can be recovered. Replaying
/// `codex resume --last` is intentionally avoided because it makes multiple
/// restored tabs collapse into the same newest conversation.
fn codex_resume(pid: c_int, cwd: &Path, ai_resume: &AiResumeConfig) -> Option<String> {
    if let Some(uuid) = codex_resume_arg(pid) {
        return Some(codex_resume_command(&uuid, ai_resume));
    }
    if let Some(uuid) = codex_session_for_pid(pid, cwd) {
        return Some(codex_resume_command(&uuid, ai_resume));
    }
    None
}

fn codex_resume_command(session_id: &str, ai_resume: &AiResumeConfig) -> String {
    build_command("codex", &ai_resume.codex.flags, ["resume", session_id])
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
    if program.ends_with("claude") {
        return claude_resume_id_from_args(&args).map(|id| claude_resume_command(&id, ai_resume));
    }
    if program.ends_with("copilot") {
        return copilot_resume_id_from_args(&args).map(|id| copilot_resume_command(&id, ai_resume));
    }
    if program.ends_with("codex") {
        return codex_resume_id_from_args(&args).map(|id| codex_resume_command(&id, ai_resume));
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
    let start_tvsec = crate::macos::proc::start_tvsec(pid)?;
    let cwd_str = cwd.to_str()?;

    let mut sessions_root = home::home_dir()?;
    sessions_root.push(".codex");
    sessions_root.push("sessions");

    codex_session_for_pid_with(&sessions_root, start_tvsec, cwd_str)
}

/// Testable inner implementation. Accepts the sessions root directory and
/// process start timestamp as parameters so tests can work with a `TempDir`
/// and fabricated timestamps without calling into the kernel or `home_dir`.
fn codex_session_for_pid_with(
    sessions_root: &Path,
    start_tvsec: u64,
    cwd_str: &str,
) -> Option<String> {
    // Walk the date dirs touched by the accepted rollout window. Candidates
    // must be within 10 minutes after process start, so wall-clock "today"
    // is irrelevant and makes long-running sessions/test fixtures stale.
    let start_dt = chrono::DateTime::from_timestamp(i64::try_from(start_tvsec).ok()?, 0)?;
    let window_end = start_dt + chrono::Duration::seconds(600);
    let date_dirs =
        [date_dir_for(sessions_root, start_dt), date_dir_for(sessions_root, window_end)];

    // We want the rollout this codex process CREATED — which must have a
    // timestamp at or after the process start. Picking "closest |Δt|" is
    // wrong for multi-codex-same-cwd: a slightly-older rollout written by
    // a sibling codex can win for both PIDs. Pick the EARLIEST rollout
    // whose timestamp is >= process start. UUIDs already claimed by a
    // sibling window in the same save-tick are skipped, so two codex
    // tabs that started in the same second still resolve to distinct
    // sessions.
    let claimed: HashSet<String> = claimed_set().lock().map(|g| g.clone()).unwrap_or_default();
    let mut earliest: Option<(u64, String)> = None;
    for dir in date_dirs.iter().flatten() {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !fname.starts_with("rollout-") || !fname.ends_with(".jsonl") {
                continue;
            }
            // `rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl`
            let body = &fname["rollout-".len()..fname.len() - ".jsonl".len()];
            if body.len() <= 20 {
                continue;
            }
            let ts_part = &body[..19];
            let uuid_part = &body[20..];
            let file_ts = match parse_codex_ts(ts_part) {
                Some(t) => t,
                None => continue,
            };
            // Must be at or after process start AND within 10 minutes.
            // 10-minute window accommodates a slow codex startup or a
            // delayed first user prompt that triggers rollout creation.
            if file_ts < start_tvsec || file_ts > start_tvsec + 600 {
                continue;
            }
            if claimed.contains(uuid_part) {
                continue;
            }
            if !rollout_matches_cwd_and_user(&path, cwd_str) {
                continue;
            }
            if earliest.as_ref().is_none_or(|(t, _)| file_ts < *t) {
                earliest = Some((file_ts, uuid_part.to_string()));
            }
        }
    }
    if let Some((_, uuid)) = earliest.as_ref() {
        if let Ok(mut g) = claimed_set().lock() {
            g.insert(uuid.clone());
        }
    }
    earliest.map(|(_, uuid)| uuid)
}

fn date_dir_for(root: &Path, dt: chrono::DateTime<chrono::Utc>) -> Option<std::path::PathBuf> {
    use chrono::Datelike;
    // Codex anchors both its date dirs and rollout-filename timestamps to
    // LOCAL time, not UTC (verified empirically — a Chicago user running
    // codex at 20:08 local writes files into `/2026/05/21/` even though
    // UTC is already 01:08 of the next day).
    let local = dt.with_timezone(&chrono_tz::America::Chicago);
    let mut p = root.to_path_buf();
    p.push(format!("{:04}", local.year()));
    p.push(format!("{:02}", local.month()));
    p.push(format!("{:02}", local.day()));
    Some(p)
}

fn parse_codex_ts(s: &str) -> Option<u64> {
    // Format: "YYYY-MM-DDTHH-MM-SS". Codex emits these in the user's
    // local timezone (NOT UTC). For a Chicago user that's CST/CDT —
    // chrono-tz handles DST automatically.
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H-%M-%S").ok()?;
    let dt = chrono_tz::America::Chicago.from_local_datetime(&naive).single()?;
    Some(dt.timestamp() as u64)
}

fn rollout_matches_cwd_and_user(path: &Path, cwd: &str) -> bool {
    use std::io::{BufRead, BufReader};
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut line = String::new();
    if BufReader::new(f).read_line(&mut line).is_err() {
        return false;
    }
    let v: serde_json::Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let payload = match v.get("payload") {
        Some(p) => p,
        None => return false,
    };
    let file_cwd = payload.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
    if file_cwd != cwd {
        return false;
    }
    // Reject subagent rollouts — only user-initiated sessions are
    // meaningful to resume.
    if let Some(source) = payload.get("source") {
        if source.is_object() && source.get("subagent").is_some() {
            return false;
        }
    }
    true
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
        assert!(is_codex_binary(
            "/Users/x/.config/yarn/global/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/codex/codex"
        ));
        assert!(is_codex_binary("/x/@openai/codex/foo"));
        assert!(!is_codex_binary("/usr/bin/zsh"));
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
            codex_resume_command("codex-session", &config),
            "codex --toml-codex-flag resume codex-session"
        );
    }

    #[test]
    fn resume_commands_use_configured_flags() {
        let mut config = test_ai_resume_config();
        config.codex.flags = vec!["--sandbox".into(), "workspace-write".into()];
        assert_eq!(
            codex_resume_command("codex-session", &config),
            "codex --sandbox workspace-write resume codex-session"
        );
    }

    #[test]
    fn resume_command_quotes_configured_flags_for_shell() {
        let mut config = test_ai_resume_config();
        config.codex.flags = vec!["--config".into(), "model=\"gpt-5 codex\"".into()];
        assert_eq!(
            codex_resume_command("codex-session", &config),
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

    // ── parse_codex_ts ────────────────────────────────────────────────────

    #[test]
    fn parse_codex_ts_valid_cst() {
        // 2026-01-15 10:00:00 CST (UTC-6) = 2026-01-15 16:00:00 UTC.
        // Verified: chrono-tz parses this to unix 1768492800.
        let ts = parse_codex_ts("2026-01-15T10-00-00");
        assert!(ts.is_some(), "should parse valid CST timestamp");
        let v = ts.unwrap();
        assert_eq!(v, 1768492800, "unexpected unix timestamp: {v}");
    }

    #[test]
    fn parse_codex_ts_valid_cdt() {
        // 2026-07-04 12:00:00 CDT (UTC-5) = 2026-07-04 17:00:00 UTC.
        // Verified: chrono-tz parses this to unix 1783184400.
        let ts = parse_codex_ts("2026-07-04T12-00-00");
        assert!(ts.is_some(), "should parse valid CDT timestamp");
        let v = ts.unwrap();
        assert_eq!(v, 1783184400, "unexpected unix timestamp: {v}");
    }

    #[test]
    fn parse_codex_ts_spring_forward_nonexistent_time_returns_none() {
        // 2026-03-08 02:30:00 local (Chicago) doesn't exist — clocks spring
        // forward from 02:00 to 03:00. `single()` returns None.
        let ts = parse_codex_ts("2026-03-08T02-30-00");
        assert!(ts.is_none(), "nonexistent DST spring-forward time should return None");
    }

    #[test]
    fn parse_codex_ts_fall_back_ambiguous_time_returns_none() {
        // 2026-11-01 01:30:00 local (Chicago) is ambiguous — clocks fall
        // back from 02:00 to 01:00, so 01:30 occurs twice. `single()` returns None.
        let ts = parse_codex_ts("2026-11-01T01-30-00");
        assert!(ts.is_none(), "ambiguous DST fall-back time should return None");
    }

    #[test]
    fn parse_codex_ts_malformed_returns_none() {
        assert!(parse_codex_ts("not-a-timestamp").is_none());
        assert!(parse_codex_ts("").is_none());
        assert!(parse_codex_ts("2026/01/01 10:00:00").is_none());
    }

    // ── codex_session_for_pid_with ────────────────────────────────────────

    /// Build a session file tree and return the sessions root path.
    ///
    /// Each entry in `rollouts` is:
    ///   `(date_subpath, ts_str, uuid, cwd, is_subagent)`
    ///
    /// `date_subpath` is relative under `sessions_root`, e.g. `"2026/05/21"`.
    fn make_sessions_root(
        tmp: &tempfile::TempDir,
        rollouts: &[(&str, &str, &str, &str, bool)],
    ) -> std::path::PathBuf {
        let root = tmp.path().join("sessions");
        for (date_sub, ts, uuid, cwd, is_subagent) in rollouts {
            let dir = root.join(date_sub);
            std::fs::create_dir_all(&dir).unwrap();
            let fname = format!("rollout-{}-{}.jsonl", ts, uuid);
            let source = if *is_subagent { r#"{"subagent": true}"# } else { r#""user""# };
            let line = format!(
                r#"{{"type":"session_meta","payload":{{"cwd":"{}","source":{}}}}}"#,
                cwd, source
            );
            std::fs::write(dir.join(fname), line).unwrap();
        }
        root
    }

    /// Parse a Chicago-local timestamp string ("YYYY-MM-DDTHH-MM-SS") to unix seconds.
    /// Panics if the timestamp is invalid — test helper only.
    fn ts_to_unix(s: &str) -> u64 {
        parse_codex_ts(s).unwrap_or_else(|| panic!("bad ts: {s}"))
    }

    #[test]
    fn earliest_after_start_wins() {
        // Two rollouts for the same cwd: one at T+1, one at T+60. The one at T+1 wins.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        // T+1 = start+1 s, T+60 = start+60s.
        // Build filenames from Chicago local equivalents of start+1 and start+60.
        // Since start is in CDT (UTC-5), start_ts = 10:00 CDT = 15:00 UTC.
        // start+1: ts string "2026-05-21T10-00-01" → parse_codex_ts would give start_ts+1,
        // but parse_codex_ts only handles exact local-time strings; use distinct minute offsets.
        let ts_early = "2026-05-21T10-01-00"; // 10:01 CDT = start+60s
        let ts_late = "2026-05-21T10-05-00"; // 10:05 CDT = start+300s
        let root = make_sessions_root(
            &tmp,
            &[
                ("2026/05/21", ts_early, "uuid-early", cwd, false),
                ("2026/05/21", ts_late, "uuid-late", cwd, false),
            ],
        );
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, start_ts, cwd).unwrap();
        assert_eq!(result, "uuid-early", "earlier timestamp should win");
    }

    #[test]
    fn cwd_mismatch_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        let root = make_sessions_root(
            &tmp,
            &[("2026/05/21", "2026-05-21T10-01-00", "uuid-wrong-cwd", "/other/project", false)],
        );
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, start_ts, "/Users/x/project");
        assert!(result.is_none(), "cwd mismatch should be rejected");
    }

    #[test]
    fn subagent_source_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        let root = make_sessions_root(
            &tmp,
            &[("2026/05/21", "2026-05-21T10-01-00", "uuid-subagent", cwd, true)],
        );
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, start_ts, cwd);
        assert!(result.is_none(), "subagent rollout should be rejected");
    }

    #[test]
    fn rollout_before_process_start_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        // Rollout at 09:58 is 2 minutes BEFORE process start.
        let root = make_sessions_root(
            &tmp,
            &[("2026/05/21", "2026-05-21T09-58-00", "uuid-before", cwd, false)],
        );
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, start_ts, cwd);
        assert!(result.is_none(), "rollout before process start should be rejected");
    }

    #[test]
    fn rollout_more_than_10min_after_start_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        // Rollout at 10:11 = start + 11 min > 10-min window.
        let root = make_sessions_root(
            &tmp,
            &[("2026/05/21", "2026-05-21T10-11-00", "uuid-too-late", cwd, false)],
        );
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, start_ts, cwd);
        assert!(result.is_none(), "rollout >10 min after start should be rejected");
    }

    #[test]
    fn claimed_uuid_is_not_returned_twice() {
        // Two calls with the same sessions root simulate two tabs with the same cwd.
        // The first call claims the earliest UUID; the second must return the next.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = "/Users/x/project";
        let start_ts = ts_to_unix("2026-05-21T10-00-00");
        let root = make_sessions_root(
            &tmp,
            &[
                ("2026/05/21", "2026-05-21T10-01-00", "uuid-first", cwd, false),
                ("2026/05/21", "2026-05-21T10-02-00", "uuid-second", cwd, false),
            ],
        );
        // Reset claimed set as begin_save_tick would do at the start of a save cycle.
        begin_save_tick();
        let first = codex_session_for_pid_with(&root, start_ts, cwd);
        // Don't call begin_save_tick between tabs — same save tick.
        let second = codex_session_for_pid_with(&root, start_ts, cwd);
        assert_eq!(first.as_deref(), Some("uuid-first"), "first tab should get earliest");
        assert_eq!(second.as_deref(), Some("uuid-second"), "second tab should get next");
    }

    #[test]
    fn no_rollouts_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        begin_save_tick();
        let result = codex_session_for_pid_with(&root, 1_000_000, "/any/cwd");
        assert!(result.is_none());
    }
}
