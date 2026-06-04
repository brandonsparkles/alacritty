//! Daily usage budget runtime state for Alacritty (macOS only).
//!
//! Policy (cap, sleep window, timezone, kill switch) lives in
//! `crate::config::budget::BudgetConfig` and is passed in at every call.
//! This module only owns runtime counters that persist across restarts:
//!
//!   * `active_seconds` — focused-time accumulated today
//!   * `weekly_active_seconds` — focused-time accumulated this week
//!   * `date_chicago` — day key the state was last written under
//!   * `courtesy_used` / `courtesy_expires_at` — optional once-per-day extension
//!   * `weekly_extension_*` — optional weekly one-hour extensions
//!
//! State lives at `~/Library/Application Support/org.alacritty/usage.json`
//! and is the single source of truth — read by the lockout overlay, the
//! localhost HTTP daemon, and the out-of-process pomodoro app.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Duration, NaiveTime, TimeZone, Timelike};
use chrono_tz::America::Chicago;
use chrono_tz::Tz;
use log::debug;
use serde::{Deserialize, Serialize};

use crate::config::budget::BudgetConfig;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeeklyExtensionError {
    Disabled,
    SleepWindow,
    AlreadyActive,
    AllowanceSpent,
}

/// Persisted budget runtime state. On-disk JSON layout is kept stable —
/// out-of-process consumers (the localhost HTTP daemon, the Tauri menu-bar
/// app) read this file directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    /// Day key (YYYY-MM-DD using a `sleep_end_hour` reset cutoff in the
    /// configured timezone) the state was last written under. Used to
    /// detect day rollover at load time.
    pub date_chicago: String,
    /// Seconds of focused time accumulated today.
    pub active_seconds: u64,
    /// Seconds of focused time accumulated this week.
    #[serde(default)]
    pub weekly_active_seconds: u64,
    /// True once the optional one-time-per-day courtesy has been spent.
    pub courtesy_used: bool,
    /// Unix-seconds timestamp when an active courtesy extension expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub courtesy_expires_at: Option<u64>,
    /// ISO week key the weekly extension allowance was last written under.
    #[serde(default)]
    pub weekly_extension_week: String,
    /// Weekly extension seconds redeemed in the current week.
    #[serde(default)]
    pub weekly_extension_used_seconds: u64,
    /// Unix-seconds timestamp when an active weekly extension expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_extension_expires_at: Option<u64>,
    /// Updated on every save. External tools use this for freshness.
    pub updated_at: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            date_chicago: String::new(),
            active_seconds: 0,
            weekly_active_seconds: 0,
            courtesy_used: false,
            courtesy_expires_at: None,
            weekly_extension_week: String::new(),
            weekly_extension_used_seconds: 0,
            weekly_extension_expires_at: None,
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
        if budget.weekly_extension_week.is_empty() {
            budget.weekly_extension_week = current_week_key(cfg);
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
            debug!("Budget: day rollover {} -> {}, resetting counter", self.date_chicago, today);
            self.date_chicago = today;
            self.active_seconds = 0;
            self.courtesy_used = false;
            self.courtesy_expires_at = None;
        }
        self.refresh_week_boundary(cfg);
    }

    /// Reset the weekly extension allowance if the configured week key
    /// has advanced.
    pub fn refresh_week_boundary(&mut self, cfg: &BudgetConfig) {
        let week = current_week_key(cfg);
        if self.weekly_extension_week != week {
            debug!(
                "Budget: week rollover {} -> {}, resetting weekly extensions",
                self.weekly_extension_week, week
            );
            self.weekly_extension_week = week;
            self.weekly_active_seconds = 0;
            self.weekly_extension_used_seconds = 0;
            self.weekly_extension_expires_at = None;
        }
    }

    /// Add `delta_seconds` to the active counter. Caller is responsible
    /// for only calling this when the app is actually front-most.
    pub fn tick(&mut self, cfg: &BudgetConfig, delta_seconds: u64) {
        self.refresh_day_boundary(cfg);
        self.active_seconds = self.active_seconds.saturating_add(delta_seconds);
        self.weekly_active_seconds = self.weekly_active_seconds.saturating_add(delta_seconds);
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
        if self.active_seconds >= cfg.cap_seconds
            && !self.courtesy_active(cfg)
            && !self.weekly_extension_active(cfg)
        {
            return Some(BlockReason::BudgetExhausted);
        }
        None
    }

    /// True when an enabled, unexpired courtesy extension is currently in effect.
    pub fn courtesy_active(&self, cfg: &BudgetConfig) -> bool {
        if !cfg.allow_courtesy || cfg.courtesy_seconds == 0 {
            return false;
        }
        match self.courtesy_expires_at {
            Some(expires_at) => unix_now() < expires_at,
            None => false,
        }
    }

    /// True when an enabled, unexpired weekly extension is currently in effect.
    pub fn weekly_extension_active(&self, cfg: &BudgetConfig) -> bool {
        if !cfg.allow_weekly_extensions {
            return false;
        }
        match self.weekly_extension_expires_at {
            Some(expires_at) => unix_now() < expires_at,
            None => false,
        }
    }

    /// Weekly extension seconds still available for the current week.
    pub fn weekly_extension_remaining_seconds(&self, cfg: &BudgetConfig) -> u64 {
        cfg.weekly_extension_allowance_seconds.saturating_sub(self.weekly_extension_used_seconds)
    }

    /// True when a one-hour weekly extension can be redeemed right now.
    pub fn weekly_extension_available(&self, cfg: &BudgetConfig) -> bool {
        cfg.allow_weekly_extensions
            && cfg.weekly_extension_seconds > 0
            && !in_sleep_window(cfg)
            && !self.weekly_extension_active(cfg)
            && self.weekly_extension_remaining_seconds(cfg) >= cfg.weekly_extension_seconds
    }

    /// Spend the optional once-per-day courtesy extension. Returns
    /// true on success, false if disabled, already used, or if the sleep
    /// window is in effect (no courtesy during the configured sleep window).
    pub fn grant_courtesy(&mut self, cfg: &BudgetConfig) -> bool {
        if !cfg.allow_courtesy || cfg.courtesy_seconds == 0 {
            return false;
        }
        if self.courtesy_used {
            return false;
        }
        if in_sleep_window(cfg) {
            return false;
        }
        self.courtesy_used = true;
        self.courtesy_expires_at = Some(unix_now() + cfg.courtesy_seconds);
        true
    }

    /// Spend one weekly extension increment.
    pub fn grant_weekly_extension(
        &mut self,
        cfg: &BudgetConfig,
    ) -> Result<(), WeeklyExtensionError> {
        self.refresh_week_boundary(cfg);
        if !cfg.allow_weekly_extensions || cfg.weekly_extension_seconds == 0 {
            return Err(WeeklyExtensionError::Disabled);
        }
        if in_sleep_window(cfg) {
            return Err(WeeklyExtensionError::SleepWindow);
        }
        if self.weekly_extension_active(cfg) {
            return Err(WeeklyExtensionError::AlreadyActive);
        }
        if self.weekly_extension_remaining_seconds(cfg) < cfg.weekly_extension_seconds {
            return Err(WeeklyExtensionError::AllowanceSpent);
        }

        self.weekly_extension_used_seconds =
            self.weekly_extension_used_seconds.saturating_add(cfg.weekly_extension_seconds);
        self.weekly_extension_expires_at = Some(unix_now() + cfg.weekly_extension_seconds);
        Ok(())
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
/// `sleep_end_hour`. Local time before the reset hour still maps to
/// *yesterday's* date.
fn current_day_key(cfg: &BudgetConfig) -> String {
    let shifted = now_in(cfg) - Duration::hours(cfg.sleep_end_hour as i64);
    shifted.format("%Y-%m-%d").to_string()
}

fn current_week_key(cfg: &BudgetConfig) -> String {
    let shifted = now_in(cfg) - Duration::hours(cfg.sleep_end_hour as i64);
    let week = shifted.iso_week();
    format!("{}-W{:02}", week.year(), week.week())
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
    seconds_until_hour(now, tz, cfg.sleep_end_hour)
}

fn seconds_until_next_day(cfg: &BudgetConfig) -> u64 {
    let tz = resolve_tz(&cfg.timezone);
    let now = chrono::Utc::now().with_timezone(&tz);
    seconds_until_hour(now, tz, cfg.sleep_end_hour)
}

fn seconds_until_hour(now: chrono::DateTime<Tz>, tz: Tz, hour: u8) -> u64 {
    let sleep_end = NaiveTime::from_hms_opt(hour as u32, 0, 0).expect("valid time");
    let today = now.date_naive();
    let candidate = today.and_time(sleep_end);
    let next_start = if (candidate - now.naive_local()).num_seconds() > 0 {
        candidate
    } else {
        let tomorrow = today.succ_opt().unwrap_or(today);
        tomorrow.and_time(sleep_end)
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
    fn default_sleep_window_is_two_to_eight_chicago_with_courtesy() {
        let cfg = BudgetConfig::default();
        assert_eq!(cfg.sleep_start_hour, 2);
        assert_eq!(cfg.sleep_end_hour, 8);
        assert_eq!(cfg.timezone, "America/Chicago");
        assert!(cfg.allow_courtesy);
        assert_eq!(cfg.courtesy_seconds, 15 * 60);
        assert!(cfg.allow_weekly_extensions);
        assert_eq!(cfg.weekly_extension_seconds, 60 * 60);
        assert_eq!(cfg.weekly_extension_allowance_seconds, 6 * 60 * 60);
    }

    #[test]
    fn budget_exhaustion_unlocks_at_next_reset_hour() {
        let cfg = BudgetConfig::default();
        let tz = resolve_tz(&cfg.timezone);
        let now = tz.with_ymd_and_hms(2026, 5, 22, 21, 30, 0).single().unwrap();
        assert_eq!(seconds_until_hour(now, tz, cfg.sleep_end_hour), 10 * 60 * 60 + 30 * 60);
    }

    #[test]
    fn courtesy_can_be_disabled_explicitly() {
        let mut cfg = BudgetConfig::default();
        cfg.allow_courtesy = false;
        cfg.sleep_start_hour = 0;
        cfg.sleep_end_hour = 0;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.active_seconds = cfg.cap_seconds;
        b.courtesy_used = false;
        b.courtesy_expires_at = Some(unix_now() + cfg.courtesy_seconds);

        assert!(!b.courtesy_active(&cfg));
        assert_eq!(b.block_status(&cfg), Some(BlockReason::BudgetExhausted));
        assert!(!b.grant_courtesy(&cfg));
    }

    #[test]
    fn courtesy_is_enabled_by_default() {
        let mut cfg = BudgetConfig::default();
        cfg.sleep_start_hour = 0;
        cfg.sleep_end_hour = 0;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.active_seconds = cfg.cap_seconds;

        assert!(b.grant_courtesy(&cfg));
        assert!(b.courtesy_active(&cfg));
        assert_eq!(b.block_status(&cfg), None);
    }

    #[test]
    fn courtesy_uses_configured_duration() {
        let mut cfg = BudgetConfig::default();
        cfg.courtesy_seconds = 42;
        cfg.sleep_start_hour = 0;
        cfg.sleep_end_hour = 0;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.active_seconds = cfg.cap_seconds;

        let before = unix_now();
        assert!(b.grant_courtesy(&cfg));
        let expires_at = b.courtesy_expires_at.expect("courtesy expiration");
        assert!(expires_at >= before + cfg.courtesy_seconds);
        assert!(expires_at <= unix_now() + cfg.courtesy_seconds);
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
        b.weekly_extension_week = "1970-W01".into();
        b.weekly_extension_used_seconds = 6 * 60 * 60;
        b.weekly_extension_expires_at = Some(unix_now() + 60);
        b.refresh_day_boundary(&cfg);
        assert_eq!(b.active_seconds, 0);
        assert!(!b.courtesy_used);
        assert_eq!(b.weekly_extension_used_seconds, 0);
        assert!(b.weekly_extension_expires_at.is_none());
    }

    #[test]
    fn weekly_extension_grants_one_hour_and_tracks_allowance() {
        let mut cfg = BudgetConfig::default();
        cfg.sleep_start_hour = 0;
        cfg.sleep_end_hour = 0;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.weekly_extension_week = current_week_key(&cfg);
        b.active_seconds = cfg.cap_seconds;

        assert_eq!(b.weekly_extension_remaining_seconds(&cfg), 6 * 60 * 60);
        assert_eq!(b.grant_weekly_extension(&cfg), Ok(()));
        assert!(b.weekly_extension_active(&cfg));
        assert_eq!(b.weekly_extension_used_seconds, 60 * 60);
        assert_eq!(b.weekly_extension_remaining_seconds(&cfg), 5 * 60 * 60);
        assert_eq!(b.block_status(&cfg), None);
    }

    #[test]
    fn weekly_extension_stops_after_six_hours() {
        let mut cfg = BudgetConfig::default();
        cfg.sleep_start_hour = 0;
        cfg.sleep_end_hour = 0;
        let mut b = Budget::default();
        b.date_chicago = current_day_key(&cfg);
        b.weekly_extension_week = current_week_key(&cfg);
        b.weekly_extension_used_seconds = cfg.weekly_extension_allowance_seconds;

        assert_eq!(b.grant_weekly_extension(&cfg), Err(WeeklyExtensionError::AllowanceSpent));
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
