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
//! | claude  | `~/.claude/sessions/<pid>.json` → `sessionId`         | `claude --resume <sessionId>`    |
//! | copilot | `~/.copilot/logs/process-<ts>-<pid>.log` last         | `copilot --resume=<sessionId>`   |
//! |         | "Registering foreground session: <uuid>" entry        |                                  |
//! | codex   | (no per-PID metadata — see fallback)                  | `codex resume --last`            |
//!
//! Tools are matched by their resolved binary path (via `proc_pidpath`),
//! not `pbi_comm`, because `pbi_comm` is truncated to 15 chars and reflects
//! the symlink target's basename (e.g. claude shows as `"2.1.144"`).
//!
//! ## Codex caveat
//!
//! codex stores sessions as `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`
//! with no PID-keyed marker we can use to map a running codex process back to
//! its specific session. The `codex resume --last` fallback picks the most
//! recently recorded session — fine for one-tab-per-user usage, but loses
//! per-tab fidelity when multiple codex tabs are running.

#![cfg(target_os = "macos")]

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::raw::c_int;
use std::path::Path;

/// Resolve a resume command for any allowlisted AI CLI that's a direct
/// child of `shell_pid`. The first matching child wins.
pub fn resume_command_for(shell_pid: c_int, _shell_cwd: &Path) -> Option<String> {
    let children = crate::macos::proc::list_children(shell_pid);
    debug_log(format_args!("resume_command_for shell_pid={} children={:?}\n", shell_pid, children));
    for child in children {
        let path = match crate::macos::proc::pid_path(child) {
            Some(p) => p,
            None => {
                debug_log(format_args!("  child={} pid_path=None\n", child));
                continue;
            }
        };
        let Some(path_str) = path.to_str() else {
            debug_log(format_args!("  child={} path-not-utf8\n", child));
            continue;
        };
        debug_log(format_args!("  child={} path={}\n", child, path_str));
        if is_claude_binary(path_str) {
            let cmd = claude_resume(child);
            debug_log(format_args!("    claude_resume({}) = {:?}\n", child, cmd));
            if let Some(cmd) = cmd {
                return Some(cmd);
            }
        } else if is_copilot_binary(path_str) {
            if let Some(cmd) = copilot_resume(child) {
                return Some(cmd);
            }
        } else if is_codex_binary(path_str) {
            return Some(codex_resume());
        }
    }
    None
}

/// Temporary diagnostic logger — appends to
/// ~/Library/Logs/Alacritty/resume-debug.log. Remove once the multi-window
/// resolver bug is understood.
fn debug_log(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let Some(mut p) = home::home_dir() else { return };
    p.push("Library");
    p.push("Logs");
    p.push("Alacritty");
    let _ = std::fs::create_dir_all(&p);
    p.push("resume-debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_fmt(args);
    }
}

// ---------- claude ----------

fn is_claude_binary(p: &str) -> bool {
    p.contains("/.claude/versions/") || p.contains("/claude/versions/")
}

/// claude maintains `~/.claude/sessions/<pid>.json` with `sessionId` while
/// the process is alive. We read it directly — minimal-parse JSON.
fn claude_resume(pid: c_int) -> Option<String> {
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
    Some(format!("claude --resume {}", session_id))
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
fn copilot_resume(pid: c_int) -> Option<String> {
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
    Some(format!("copilot --resume={}", uuid))
}

// ---------- codex ----------

fn is_codex_binary(p: &str) -> bool {
    p.contains("@openai/codex") || p.contains("/codex-darwin-")
}

/// codex provides no PID-keyed metadata, so we use the user-level "last"
/// fallback. Two parallel codex tabs in the same cwd will both restore to
/// the same most-recent session — see module docstring.
fn codex_resume() -> String {
    "codex resume --last".to_string()
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
}
