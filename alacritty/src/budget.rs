//! Daily usage budget runtime state for Alacritty (macOS only).
//!
//! Policy (cap, sleep window, timezone, kill switch) lives in
//! `crate::config::budget::BudgetConfig` and is passed in at every call.
//! This module only owns runtime counters that persist across restarts:
//!
//!   * `active_seconds` — focused-time accumulated today
//!   * `date_chicago` — day key the state was last written under
//!   * `courtesy_used` / `courtesy_expires_at` — once-per-day extension
//!
//! State lives at `~/Library/Application Support/org.alacritty/usage.json`
//! and is the single source of truth — read by the lockout overlay, the
//! localhost HTTP daemon, and the out-of-process pomodoro app.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Duration, NaiveTime, TimeZone, Timelike};
use chrono_tz::America::Chicago;
use chrono_tz::Tz;
use log::debug;
use serde::{Deserialize, Serialize};

use crate::config::budget::BudgetConfig;

/// Courtesy extension grants this many additional active seconds.
const COURTESY_DURATION_SECONDS: u64 = 5 * 60;

/// Reason a block is currently active. Stable wire contract — mirrored
/// over the HTTP daemon for the pomodoro/Tauri UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// Local time is inside the sleep window
    /// (`sleep_start_hour` .. `sleep_end_hour`).
    SleepWindow,
    /// Active seconds have reached the daily cap.
    BudgetExhausted,
}

/// Persisted budget runtime state. On-disk JSON layout is kept stable —
/// out-of-process consumers (the localhost HTTP daemon, the Tauri menu-bar
/// app) read this file directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    /// Day key (YYYY-MM-DD using a `sleep_start_hour` cutoff in the
    /// configured timezone) the state was last written under. Used to
    /// detect day rollover at load time.
    pub date_chicago: String,
    /// Seconds of focused time accumulated today.
    pub active_seconds: u64,
    /// True once the one-time-per-day 5-minute courtesy has been spent.
    pub courtesy_used: bool,
    /// Unix-seconds timestamp when an active courtesy extension expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub courtesy_expires_at: Option<u64>,
    /// Updated on every save. External tools use this for freshness.
    pub updated_at: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            date_chicago: String::new(),
            active_seconds: 0,
            courtesy_used: false,
            courtesy_expires_at: None,
            updated_at: unix_now(),
        }
    }
}

impl Budget {
    /// Path to the JSON state file.
    pub fn path() -> Option<PathBuf> {
        let mut p = home::home_dir()?;
        p.push("Library");
        p.push("Application Support");
        p.push("org.alacritty");
        p.push("usage.json");
        Some(p)
    }

    /// Load the persisted state, or return a fresh default if absent /
    /// unreadable / malformed. Rolls over the day at load time using the
    /// supplied policy.
    pub fn load_or_default(cfg: &BudgetConfig) -> Self {
        let mut budget = Self::path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            .unwrap_or_default();
        // First-ever load: stamp today's date key so rollover logic has
        // something to compare against on the next tick.
        if budget.date_chicago.is_empty() {
            budget.date_chicago = current_day_key(cfg);
        }
        budget.refresh_day_boundary(cfg);
        budget
    }

    /// Persist atomically (temp file + rename). Errors are logged but
    /// never propagated — the budget keeps running on the in-memory
    /// state even if disk writes fail.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                debug!("Could not create budget dir {}: {err}", parent.display());
                return;
            }
        }
        let mut snapshot = self.clone();
        snapshot.updated_at = unix_now();
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(s) => s,
            Err(err) => {
                debug!("Could not serialize budget: {err}");
                return;
            },
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = fs::write(&tmp, json) {
            debug!("Could not write {}: {err}", tmp.display());
            return;
        }
        if let Err(err) = fs::rename(&tmp, &path) {
            debug!("Could not rename {} -> {}: {err}", tmp.display(), path.display());
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Reset the day's accumulators if the current day key has advanced
    /// past the one in `date_chicago`.
    pub fn refresh_day_boundary(&mut self, cfg: &BudgetConfig) {
        let today = current_day_key(cfg);
        if self.date_chicago != today {
            debug!(
                "Budget: day rollover {} -> {}, resetting counter",
                self.date_chicago, today
            );
            self.date_chicago = today;
            self.active_seconds = 0;
            self.courtesy_used = false;
            self.courtesy_expires_at = None;
        }
    }

    /// Add `delta_seconds` to the active counter. Caller is responsible
    /// for only calling this when the app is actually front-most.
    pub fn tick(&mut self, cfg: &BudgetConfig, delta_seconds: u64) {
        self.refresh_day_boundary(cfg);
        self.active_seconds = self.active_seconds.saturating_add(delta_seconds);
    }

    /// Returns `Some(reason)` if the user should be locked out right
    /// now, or `None` if Alacritty is free to accept input.
    pub fn block_status(&self, cfg: &BudgetConfig) -> Option<BlockReason> {
        if !cfg.enabled {
            return None;
        }
        if in_sleep_window(cfg) {
            return Some(BlockReason::SleepWindow);
        }
        if self.active_seconds >= cfg.cap_seconds && !self.courtesy_active() {
            return Some(BlockReason::BudgetExhausted);
        }
        None
    }

    /// True when an unexpired courtesy extension is currently in effect.
    pub fn courtesy_active(&self) -> bool {
        match self.courtesy_expires_at {
            Some(expires_at) => unix_now() < expires_at,
            None => false,
        }
    }

    /// Spend the once-per-day 5-minute courtesy extension. Returns true
    /// on success, false if already used or if the sleep window is in
    /// effect (no courtesy during 02:00–06:00).
    pub fn grant_courtesy(&mut self, cfg: &BudgetConfig) -> bool {
        if self.courtesy_used {
            return false;
        }
        if in_sleep_window(cfg) {
            return false;
        }
        self.courtesy_used = true;
        self.courtesy_expires_at = Some(unix_now() + COURTESY_DURATION_SECONDS);
        true
    }

    /// Seconds remaining before the user is unblocked at the soonest.
    /// Returns 0 when not blocked.
    pub fn seconds_until_unlock(&self, cfg: &BudgetConfig) -> u64 {
        match self.block_status(cfg) {
            None => 0,
            Some(BlockReason::SleepWindow) => seconds_until_sleep_end(cfg),
            Some(BlockReason::BudgetExhausted) => seconds_until_next_day(cfg),
        }
    }
}

// ---------- timezone-aware helpers ----------

fn resolve_tz(name: &str) -> Tz {
    name.parse::<Tz>().unwrap_or(Chicago)
}

fn now_in(cfg: &BudgetConfig) -> chrono::DateTime<Tz> {
    chrono::Utc::now().with_timezone(&resolve_tz(&cfg.timezone))
}

/// Day key — calendar date of the budget's "day" anchored at
/// `sleep_start_hour`. Local time before sleep_start still maps to
/// *yesterday's* date.
fn current_day_key(cfg: &BudgetConfig) -> String {
    let shifted = now_in(cfg) - Duration::hours(cfg.sleep_start_hour as i64);
    shifted.format("%Y-%m-%d").to_string()
}

fn in_sleep_window(cfg: &BudgetConfig) -> bool {
    let hour = now_in(cfg).hour() as u8;
    // Handle wraparound (start>end means "spans midnight") even though we
    // don't use that today — keeps the function honest.
    if cfg.sleep_start_hour <= cfg.sleep_end_hour {
        hour >= cfg.sleep_start_hour && hour < cfg.sleep_end_hour
    } else {
        hour >= cfg.sleep_start_hour || hour < cfg.sleep_end_hour
    }
}

fn seconds_until_sleep_end(cfg: &BudgetConfig) -> u64 {
    let tz = resolve_tz(&cfg.timezone);
    let now = chrono::Utc::now().with_timezone(&tz);
    let today = now.date_naive();
    let sleep_end =
        NaiveTime::from_hms_opt(cfg.sleep_end_hour as u32, 0, 0).expect("valid time");
    let target = today.and_time(sleep_end);
    let target_tz = match tz.from_local_datetime(&target).single() {
        Some(t) => t,
        None => return 0,
    };
    let secs = (target_tz - now).num_seconds();
    if secs < 0 { 0 } else { secs as u64 }
}

fn seconds_until_next_day(cfg: &BudgetConfig) -> u64 {
    let tz = resolve_tz(&cfg.timezone);
    let now = chrono::Utc::now().with_timezone(&tz);
    let sleep_start =
        NaiveTime::from_hms_opt(cfg.sleep_start_hour as u32, 0, 0).expect("valid time");
    let today = now.date_naive();
    let candidate = today.and_time(sleep_start);
    let next_start = if (candidate - now.naive_local()).num_seconds() > 0 {
        candidate
    } else {
        let tomorrow = today.succ_opt().unwrap_or(today);
        tomorrow.and_time(sleep_start)
    };
    let target_tz = match tz.from_local_datetime(&next_start).single() {
        Some(t) => t,
        None => return 0,
    };
    let secs = (target_tz - now).num_seconds();
    if secs < 0 { 0 } else { secs as u64 }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::budget::BudgetConfig;

    #[test]
    fn default_cap_is_three_hours() {
        let cfg = BudgetConfig::default();
        assert_eq!(cfg.cap_seconds, 10_800);
    }

    #[test]
    fn tick_advances_counter() {
        let cfg = BudgetConfig::default();
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.tick(&cfg, 60);
        assert_eq!(b.active_seconds, 60);
    }

    #[test]
    fn day_rollover_resets_counter() {
        let cfg = BudgetConfig::default();
        let mut b = Budget::default();
        b.date_chicago = "1970-01-01".into();
        b.active_seconds = 5_000;
        b.courtesy_used = true;
        b.refresh_day_boundary(&cfg);
        assert_eq!(b.active_seconds, 0);
        assert!(!b.courtesy_used);
    }

    #[test]
    fn disabled_config_never_blocks() {
        let mut cfg = BudgetConfig::default();
        cfg.enabled = false;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.active_seconds = 100_000; // way past cap
        assert_eq!(b.block_status(&cfg), None);
    }

    #[test]
    fn invalid_timezone_falls_back_to_chicago() {
        let mut cfg = BudgetConfig::default();
        cfg.timezone = "Not/A_Real_Zone".to_string();
        // Should not panic; current_day_key produces *something*.
        let _ = current_day_key(&cfg);
    }
}
