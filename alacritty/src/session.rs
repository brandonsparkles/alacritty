//! macOS-only window-session persistence.
//!
//! Writes a JSON snapshot of all open windows to
//! `~/Library/Saved Application State/org.alacritty.savedState/session.json`
//! on every meaningful change. macOS clears that directory when the user
//! shift-clicks the Dock icon to "Reopen Without Restoring", so we get the
//! standard opt-out for free.
//!
//! On launch, if the file exists *and* no command/working-directory was
//! supplied on the CLI, [`Session::load`] returns the recorded state which
//! the event loop consumes to recreate the windows.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;

use log::{debug, warn};
use serde::{Deserialize, Serialize};

/// Tracked state for a single window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    /// Working directory of the shell. Used as `--working-directory` when the
    /// window is re-spawned.
    pub working_directory: PathBuf,
    /// User-set tab title (set via the inline rename prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_title: Option<String>,
    /// macOS tab grouping id — windows sharing the same value will be tabbed
    /// together on restore.
    pub tabbing_id: String,
    /// Window size in physical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<(u32, u32)>,
    /// Window position in screen coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<(i32, i32)>,
}

/// Persisted snapshot of all open windows at the time of save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Path to the persisted session file.
    ///
    /// Uses the macOS "Saved Application State" directory so the file is
    /// honoured by the system's "Reopen Without Restoring" gesture.
    pub fn path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let mut path = PathBuf::from(home);
        path.push("Library");
        path.push("Saved Application State");
        path.push("org.alacritty.savedState");
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
    pub fn save(&self) {
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

    /// Remove the saved state. Currently unused — kept for future opt-out
    /// flows ("Clear Saved State" menu item, etc.).
    #[allow(dead_code)]
    pub fn clear() {
        if let Some(file) = Self::file() {
            let _ = fs::remove_file(file);
        }
    }
}
