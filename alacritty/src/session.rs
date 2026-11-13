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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::{fs, io};

use log::{Level, log, warn};
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

/// True while the current run of consecutive save failures has already been
/// logged at `warn`. Only the save worker thread writes snapshots, so this
/// behaves as that thread's streak state; it is a static because
/// [`Session::save`] has no caller-owned state to hang it on.
static SAVE_FAILURE_WARNED: AtomicBool = AtomicBool::new(false);

/// Log level for a save failure: `Warn` for the first failure of a streak,
/// `Debug` while the streak continues. A persistent failure (disk full,
/// permissions) retries on every 3s tick, and `warn!` also lands in the
/// message bar, so repeating it would flood both. [`Session::save_to`] ends
/// the streak on its next success.
fn save_failure_level(warned: &AtomicBool) -> Level {
    if warned.swap(true, Ordering::Relaxed) { Level::Debug } else { Level::Warn }
}

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
    ///
    /// A missing file is the normal first-launch case and stays silent. Any
    /// other fallback (unreadable file, unsupported version, parse failure) is
    /// logged, because the next save tick overwrites the saved session with
    /// the fresh single-window snapshot.
    pub fn load() -> Option<Session> {
        Self::load_from(&Self::file()?)
    }

    fn load_from(file: &Path) -> Option<Session> {
        let raw = match fs::read_to_string(file) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                warn!("Failed to read session file at {}: {err}", file.display());
                return None;
            },
        };
        match serde_json::from_str::<Session>(&raw) {
            Ok(session) if session.version == CURRENT_VERSION && !session.windows.is_empty() => {
                Some(session)
            },
            Ok(session) if session.version != CURRENT_VERSION => {
                warn!(
                    "Ignoring session file at {} with version {} (expected {CURRENT_VERSION})",
                    file.display(),
                    session.version,
                );
                None
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
    pub fn save(&self) -> bool {
        let Some(dir) = Self::path() else { return false };
        self.save_to(&dir)
    }

    /// Return true only after a nonempty snapshot reaches its final path.
    fn save_to(&self, dir: &Path) -> bool {
        self.save_to_with(dir, &SAVE_FAILURE_WARNED)
    }

    /// [`Self::save_to`] with the failure-streak flag supplied by the caller.
    fn save_to_with(&self, dir: &Path, failure_warned: &AtomicBool) -> bool {
        if self.windows.is_empty() {
            return false;
        }
        let file = dir.join("session.json");

        if let Err(err) = fs::create_dir_all(dir) {
            log!(
                save_failure_level(failure_warned),
                "Could not create saved-state dir {}: {err}",
                dir.display()
            );
            return false;
        }

        let mut to_persist = self.clone();
        to_persist.version = CURRENT_VERSION;

        let json = match serde_json::to_string_pretty(&to_persist) {
            Ok(j) => j,
            Err(err) => {
                log!(save_failure_level(failure_warned), "Could not serialize session: {err}");
                return false;
            },
        };

        // Atomic write: temp file + rename so partial writes never corrupt
        // a previously-good session.
        let tmp = file.with_extension("json.tmp");
        if let Err(err) = fs::write(&tmp, json) {
            log!(save_failure_level(failure_warned), "Could not write {}: {err}", tmp.display());
            return false;
        }
        if let Err(err) = fs::rename(&tmp, &file) {
            log!(
                save_failure_level(failure_warned),
                "Could not rename {} -> {}: {err}",
                tmp.display(),
                file.display()
            );
            let _ = fs::remove_file(&tmp);
            return false;
        }
        failure_warned.store(false, Ordering::Relaxed);
        true
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
    persist_snapshot(session, last_saved, Session::save);
}

fn persist_snapshot(
    session: Session,
    last_saved: &mut Option<Session>,
    persist: impl FnOnce(&Session) -> bool,
) {
    if session.windows.is_empty() || last_saved.as_ref() == Some(&session) {
        return;
    }
    // A failed write must remain eligible for retry on the next tick.
    if persist(&session) {
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

    fn fixture_session() -> Session {
        Session {
            version: CURRENT_VERSION,
            windows: vec![WindowState {
                working_directory: PathBuf::from("/fixture"),
                tab_title: None,
                tabbing_id: String::new(),
                size: None,
                position: None,
                resume_command: None,
            }],
        }
    }

    /// Records captured by [`CaptureLogger`]: level and message.
    static CAPTURED: std::sync::Mutex<Vec<(Level, String)>> = std::sync::Mutex::new(Vec::new());

    struct CaptureLogger;

    impl log::Log for CaptureLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let mut captured = CAPTURED.lock().unwrap_or_else(|err| err.into_inner());
            captured.push((record.level(), record.args().to_string()));
        }

        fn flush(&self) {}
    }

    /// Route `log` records into [`CAPTURED`]. Tests share the process-wide logger and run in
    /// parallel, so each assertion filters on a marker unique to its own temp dir.
    fn install_capture_logger() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let _ = log::set_logger(&CaptureLogger);
            log::set_max_level(log::LevelFilter::Debug);
        });
    }

    fn captured_containing(marker: &str) -> Vec<(Level, String)> {
        let captured = CAPTURED.lock().unwrap_or_else(|err| err.into_inner());
        captured.iter().filter(|(_, message)| message.contains(marker)).cloned().collect()
    }

    #[test]
    fn load_missing_file_is_silent() {
        install_capture_logger();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.json");
        assert!(Session::load_from(&file).is_none());
        assert!(captured_containing(&dir.path().to_string_lossy()).is_empty());
    }

    #[test]
    fn load_unreadable_file_warns() {
        install_capture_logger();
        let dir = tempfile::tempdir().unwrap();
        // A directory in place of the file fails with a non-NotFound read error, as a
        // permission or I/O error would.
        let file = dir.path().join("session.json");
        fs::create_dir(&file).unwrap();
        assert!(Session::load_from(&file).is_none());
        let records = captured_containing(&dir.path().to_string_lossy());
        assert!(
            records
                .iter()
                .any(|(level, msg)| *level == Level::Warn
                    && msg.contains("Failed to read session file")),
            "unreadable session file was not warned about: {records:?}"
        );
    }

    #[test]
    fn load_unsupported_version_warns() {
        install_capture_logger();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.json");
        let mut future = fixture_session();
        future.version = CURRENT_VERSION + 1;
        fs::write(&file, serde_json::to_string(&future).unwrap()).unwrap();
        assert!(Session::load_from(&file).is_none());
        let records = captured_containing(&dir.path().to_string_lossy());
        let expected = format!("version {}", CURRENT_VERSION + 1);
        assert!(
            records.iter().any(|(level, msg)| *level == Level::Warn && msg.contains(&expected)),
            "version mismatch was not warned about: {records:?}"
        );
    }

    #[test]
    fn load_current_version_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fixture_session().save_to_with(dir.path(), &AtomicBool::new(false)));
        assert_eq!(Session::load_from(&dir.path().join("session.json")), Some(fixture_session()));
    }

    #[test]
    fn save_failure_warns_once_per_streak_and_resets_on_success() {
        install_capture_logger();
        let warned = AtomicBool::new(false);
        assert_eq!(save_failure_level(&warned), Level::Warn);
        assert_eq!(save_failure_level(&warned), Level::Debug);
        assert_eq!(save_failure_level(&warned), Level::Debug);

        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("saved");
        fs::write(&blocked, "blocked").unwrap();
        let session = fixture_session();

        // Streak already warned: the failure is still recorded, but at debug.
        assert!(!session.save_to_with(&blocked, &warned));
        let marker = blocked.to_string_lossy().into_owned();
        let records = captured_containing(&marker);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].0, Level::Debug);
        assert!(warned.load(Ordering::Relaxed));

        // A success ends the streak, so the next failure warns again.
        let good = dir.path().join("good");
        assert!(session.save_to_with(&good, &warned));
        assert!(!warned.load(Ordering::Relaxed));
        assert!(!session.save_to_with(&blocked, &warned));
        let records = captured_containing(&marker);
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[1].0, Level::Warn);
        assert!(records[1].1.contains("Could not create saved-state dir"));
    }

    #[test]
    fn failed_persistence_retries_identical_snapshot_after_recovery() {
        for failure in ["mkdir", "write", "rename"] {
            let dir = tempfile::tempdir().unwrap();
            let destination = dir.path().join("saved");
            match failure {
                "mkdir" => fs::write(&destination, "blocked").unwrap(),
                "write" => fs::create_dir_all(destination.join("session.json.tmp")).unwrap(),
                _ => fs::create_dir_all(destination.join("session.json")).unwrap(),
            }
            let session = fixture_session();
            let mut last_saved = None;
            persist_snapshot(session.clone(), &mut last_saved, |s| s.save_to(&destination));
            assert!(last_saved.is_none(), "{failure} must not mark snapshot saved");
            match failure {
                "mkdir" => fs::remove_file(&destination).unwrap(),
                "write" => fs::remove_dir(destination.join("session.json.tmp")).unwrap(),
                _ => fs::remove_dir(destination.join("session.json")).unwrap(),
            }
            persist_snapshot(session.clone(), &mut last_saved, |s| s.save_to(&destination));
            assert_eq!(last_saved.as_ref(), Some(&session));
            let persisted: Session =
                serde_json::from_slice(&fs::read(destination.join("session.json")).unwrap())
                    .unwrap();
            assert_eq!(persisted, session);
            persist_snapshot(session, &mut last_saved, |_| panic!("unchanged success must skip"));
        }
    }

    #[test]
    fn failure_and_empty_snapshot_preserve_last_successful_state() {
        let dir = tempfile::tempdir().unwrap();
        let old = fixture_session();
        assert!(old.save_to(dir.path()));
        let original = fs::read(dir.path().join("session.json")).unwrap();
        let mut last_saved = Some(old.clone());
        let mut changed = old.clone();
        changed.windows[0].tab_title = Some("changed".into());
        fs::create_dir(dir.path().join("session.json.tmp")).unwrap();
        persist_snapshot(changed.clone(), &mut last_saved, |s| s.save_to(dir.path()));
        assert_eq!(last_saved.as_ref(), Some(&old));
        assert_eq!(fs::read(dir.path().join("session.json")).unwrap(), original);
        let empty = Session::default();
        assert!(!empty.save_to(dir.path()));
        persist_snapshot(empty, &mut last_saved, |_| panic!("empty must skip"));
        assert_eq!(last_saved.as_ref(), Some(&old));
        assert_eq!(fs::read(dir.path().join("session.json")).unwrap(), original);
        fs::remove_dir(dir.path().join("session.json.tmp")).unwrap();
        persist_snapshot(changed.clone(), &mut last_saved, |s| s.save_to(dir.path()));
        assert_eq!(last_saved.as_ref(), Some(&changed));
    }
}
