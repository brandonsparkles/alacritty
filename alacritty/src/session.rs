//! macOS-only window-session persistence.
//!
//! Writes a JSON snapshot of all open windows to
//! `~/Library/Application Support/org.alacritty/session.json` on every
//! meaningful change.
//!
//! Note: this used to live inside
//! `~/Library/Saved Application State/org.alacritty.savedState/` to get
//! macOS's "Reopen Without Restoring" opt-out for free. That coupling
//! turned out to be a footgun — macOS wipes that directory after a
//! crash-loop, so an unrelated SIGABRT could erase a perfectly good session.
//! We now persist outside of macOS-managed state and implement the opt-out
//! ourselves: holding Shift at launch (see [`should_clear_on_launch`] in
//! `main.rs`) clears the saved session before [`Session::load`] runs.
//!
//! On launch, if the file exists *and* no command/working-directory was
//! supplied on the CLI, [`Session::load`] returns the recorded state which
//! the event loop consumes to recreate the windows.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError};

use log::{debug, warn};
use serde::{Deserialize, Serialize};

use crate::config::ai_resume::AiResumeConfig;

/// Tracked state for a single window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Working directory of the shell. Used as `--working-directory` when the
    /// window is re-spawned.
    pub working_directory: PathBuf,
    /// User-set tab title (set via the inline rename prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_title: Option<String>,
    /// Legacy macOS tab grouping id.
    ///
    /// Older custom builds persisted this value, but AppKit's tabbing
    /// identifiers are not stable across launches. Keep the field only for
    /// backwards-compatible parsing of existing session files.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tabbing_id: String,
    /// Window size in physical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<(u32, u32)>,
    /// Window position in screen coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<(i32, i32)>,
    /// Optional shell command to auto-execute after the restored shell
    /// reaches its first prompt. Set when the tab was running an
    /// allowlisted AI CLI (claude/codex/…) and a session identifier was
    /// recoverable. The command typically embeds the session id so the
    /// CLI resumes the prior conversation rather than starting fresh.
    /// See `cli_resume.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

/// Persisted snapshot of all open windows at the time of save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Schema version. Bumped if the layout changes incompatibly.
    #[serde(default = "default_version")]
    pub version: u32,
    pub windows: Vec<WindowState>,
}

fn default_version() -> u32 {
    1
}

const CURRENT_VERSION: u32 = 1;

impl Session {
    /// Path to the persisted session directory.
    ///
    /// Lives under `~/Library/Application Support/org.alacritty/` so the
    /// session survives macOS's saved-state wipes (e.g. after a crash loop).
    /// Opt-out for restoration is provided by the Shift-at-launch check in
    /// `main.rs`, not by macOS managing this directory.
    pub fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let mut path = PathBuf::from(home);
        path.push("Library");
        path.push("Application Support");
        path.push("org.alacritty");
        Some(path)
    }

    /// Full path to the JSON file inside the saved-state directory.
    pub fn file() -> Option<PathBuf> {
        Self::path().map(|mut p| {
            p.push("session.json");
            p
        })
    }

    /// Load the persisted session, if any. Returns `None` if no session file
    /// exists or it cannot be parsed — never an error, since failure to
    /// restore should silently fall back to "fresh window".
    pub fn load() -> Option<Session> {
        let file = Self::file()?;
        let raw = fs::read_to_string(&file).ok()?;
        match serde_json::from_str::<Session>(&raw) {
            Ok(session) if session.version == CURRENT_VERSION && !session.windows.is_empty() => {
                Some(session)
            },
            Ok(_) => None,
            Err(err) => {
                warn!("Failed to parse session file at {}: {err}", file.display());
                None
            },
        }
    }

    /// Persist this session atomically. Best-effort — logs but does not panic
    /// on filesystem failure.
    ///
    /// Empty-window snapshots are NOT written: if the user closes every
    /// window and the periodic tick fires before they quit (or while
    /// alacritty idles with no windows), we don't want to overwrite the
    /// last good state with `windows: []` — that would make the next
    /// launch restore nothing. The Shift-at-launch opt-out clears
    /// explicitly when the user really wants a fresh start.
    pub fn save(&self) {
        if self.windows.is_empty() {
            return;
        }

        let Some(dir) = Self::path() else { return };
        let Some(file) = Self::file() else { return };

        if let Err(err) = fs::create_dir_all(&dir) {
            debug!("Could not create saved-state dir {}: {err}", dir.display());
            return;
        }

        let mut to_persist = self.clone();
        to_persist.version = CURRENT_VERSION;

        let json = match serde_json::to_string_pretty(&to_persist) {
            Ok(j) => j,
            Err(err) => {
                debug!("Could not serialize session: {err}");
                return;
            },
        };

        // Atomic write: temp file + rename so partial writes never corrupt
        // a previously-good session.
        let tmp = file.with_extension("json.tmp");
        if let Err(err) = fs::write(&tmp, json) {
            debug!("Could not write {}: {err}", tmp.display());
            return;
        }
        if let Err(err) = fs::rename(&tmp, &file) {
            debug!("Could not rename {} -> {}: {err}", tmp.display(), file.display());
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Remove the saved state. Called at launch when the user holds Shift
    /// (the manual replacement for macOS's "Reopen Without Restoring"
    /// gesture, now that we no longer live in the macOS-managed dir).
    pub fn clear() {
        if let Some(file) = Self::file() {
            let _ = fs::remove_file(file);
        }
    }
}

// ---------- background save worker ----------

/// Cheap per-window facts collected on the winit event-loop thread. The
/// expensive half of a snapshot (shell cwd, proc-tree walk, fd tables,
/// copilot log scan, JSON write) is derived from this seed on the worker.
#[derive(Debug)]
pub struct WindowSeed {
    /// PID of the window's shell (PTY child).
    pub shell_pid: i32,
    /// User-set tab title override, if any.
    pub tab_title: Option<String>,
    /// Window size in physical pixels.
    pub size: Option<(u32, u32)>,
    /// Window position in screen coordinates.
    pub position: Option<(i32, i32)>,
}

/// One save tick's worth of work handed to the worker thread.
pub struct SaveRequest {
    pub windows: Vec<WindowSeed>,
    pub ai_resume: AiResumeConfig,
}

/// Background thread that turns [`SaveRequest`]s into `session.json`
/// writes, keeping proc/file scanning off the event-loop thread.
///
/// Resume commands are resolved for every request: a shell PID can stay
/// unchanged while its AI CLI starts, exits, or switches conversations.
/// The usual freshness is the 3s tick cadence plus worker processing time.
pub struct SaveWorker {
    tx: SyncSender<SaveRequest>,
}

impl SaveWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<SaveRequest>(1);
        let _ = std::thread::Builder::new().name("alacritty-session-save".to_string()).spawn(
            move || {
                // Last snapshot that hit disk; identical snapshots skip
                // the serialize + write entirely.
                let mut last_saved: Option<Session> = None;
                while let Ok(request) = rx.recv() {
                    process_save(request, &mut last_saved);
                }
            },
        );
        Self { tx }
    }

    /// Queue one save tick. If the worker is still busy with the previous
    /// request the tick is dropped — the next 3s tick retries. A busy
    /// worker can therefore delay persistence beyond the usual cadence.
    pub fn request(&self, request: SaveRequest) {
        match self.tx.try_send(request) {
            Ok(()) | Err(TrySendError::Full(_)) => (),
            Err(TrySendError::Disconnected(_)) => {
                warn!("Session save worker is gone; snapshot dropped");
            },
        }
    }
}

fn process_save(request: SaveRequest, last_saved: &mut Option<Session>) {
    // Every window resolves afresh, retaining the per-tick protection
    // against sibling windows claiming the same codex rollout.
    crate::cli_resume::begin_save_tick();
    let session = snapshot(&request, |shell_pid, config| {
        let working_directory = crate::macos::proc::cwd(shell_pid).ok()?;
        let resume_command =
            crate::cli_resume::resume_command_for(shell_pid, &working_directory, config);
        Some((working_directory, resume_command))
    });
    if last_saved.as_ref() == Some(&session) {
        return;
    }
    session.save();
    // Empty snapshots are never written (see `Session::save`), so they
    // must not update the last-written marker either.
    if !session.windows.is_empty() {
        *last_saved = Some(session);
    }
}

fn snapshot(
    request: &SaveRequest,
    mut resolve: impl FnMut(i32, &AiResumeConfig) -> Option<(PathBuf, Option<String>)>,
) -> Session {
    let mut windows = Vec::with_capacity(request.windows.len());
    for seed in &request.windows {
        // A window whose shell is gone is skipped, matching the previous
        // synchronous snapshot behavior.
        let Some((working_directory, resume_command)) = resolve(seed.shell_pid, &request.ai_resume)
        else {
            continue;
        };
        windows.push(WindowState {
            working_directory,
            tab_title: seed.tab_title.clone(),
            tabbing_id: String::new(),
            size: seed.size,
            position: seed.position,
            resume_command,
        });
    }

    Session { version: CURRENT_VERSION, windows }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_refreshes_resume_under_unchanged_shell_pid_every_request() {
        let request = SaveRequest {
            windows: vec![WindowSeed {
                shell_pid: 42,
                tab_title: Some("work".into()),
                size: Some((800, 600)),
                position: Some((10, 20)),
            }],
            ai_resume: AiResumeConfig::default(),
        };
        // No sleeping: consecutive save requests must observe start,
        // conversation switch, and exit immediately, including cached None.
        for command in [None, Some("claude --resume first"), Some("codex resume second"), None] {
            let session = snapshot(&request, |pid, _| {
                assert_eq!(pid, 42);
                Some((PathBuf::from("/fixture"), command.map(str::to_owned)))
            });
            let window = &session.windows[0];
            assert_eq!(window.resume_command.as_deref(), command);
            assert_eq!(window.tab_title.as_deref(), Some("work"));
            assert_eq!(window.size, Some((800, 600)));
            assert_eq!(window.position, Some((10, 20)));
        }
        assert!(snapshot(&request, |_, _| None).windows.is_empty());
    }
}
