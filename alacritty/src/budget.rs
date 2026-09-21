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
//! The live state is a single `Arc<Mutex<Budget>>` shared between the
//! winit event loop (which ticks it) and the localhost HTTP daemon (which
//! serves and mutates it). `~/Library/Application Support/org.alacritty/
//! usage.json` is its crash-safe persistence: written on every tick that
//! changes a counter (loss bound ≤1s while time is being counted), plus a
//! periodic `updated_at` heartbeat while idle.

#![cfg(target_os = "macos")]

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Duration, NaiveTime, TimeZone, Timelike};
use chrono_tz::America::Chicago;
use chrono_tz::Tz;
use log::{debug, warn};
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

    /// Load the persisted state.
    ///
    /// "File absent" and "file present but damaged" are deliberately NOT
    /// the same case. An absent file is a genuine first run and starts at
    /// zero. A file that exists but does not parse (truncated write, hand
    /// edit, disk error) means the day's accumulated time is unknown — so
    /// we log loudly and **fail closed**, salvaging whatever counters are
    /// still readable and assuming the cap is spent for anything that is
    /// not. Zeroing here would silently hand back a fresh 3 hours to
    /// anyone who corrupts the file.
    pub fn load_or_default(cfg: &BudgetConfig) -> Self {
        let mut budget = match Self::path() {
            Some(path) => match fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<Self>(&raw) {
                    Ok(budget) => budget,
                    Err(err) => {
                        warn!(
                            "Budget: {} is present but unparseable ({err}); failing closed \
                             instead of resetting today's counter",
                            path.display()
                        );
                        Self::fail_closed(cfg, &raw)
                    },
                },
                Err(err) if err.kind() == io::ErrorKind::NotFound => Self::default(),
                Err(err) => {
                    warn!(
                        "Budget: {} exists but could not be read ({err}); failing closed",
                        path.display()
                    );
                    Self::fail_closed(cfg, "")
                },
            },
            None => Self::default(),
        };
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

    /// Conservative reconstruction used when the state file exists but is
    /// damaged. Any counter still legible in `raw` is kept verbatim;
    /// anything unreadable is assumed spent (cap reached, courtesy used,
    /// weekly allowance exhausted) so corruption can never buy time.
    ///
    /// The salvaged day/week keys are shape-checked before being trusted.
    /// `load_or_default` runs `refresh_day_boundary` immediately after
    /// this, and that rolls over — zeroing `active_seconds` and clearing
    /// `courtesy_used` — whenever today's key sorts *above* the stored
    /// one. A damaged file carrying a non-date string (`"date_chicago":
    /// "0"`, `"!!"`, a truncated fragment) sorts below every real key, so
    /// trusting it verbatim would convert this fail-closed path into a
    /// full refund. An unparseable key therefore falls back to the current
    /// key, which cannot trigger a rollover.
    fn fail_closed(cfg: &BudgetConfig, raw: &str) -> Self {
        let salvaged: Option<serde_json::Value> = serde_json::from_str(raw).ok();
        let field = |key: &str| salvaged.as_ref().and_then(|v| v.get(key).cloned());
        let u64_field = |key: &str| field(key).and_then(|v| v.as_u64());
        let str_field = |key: &str| {
            field(key).and_then(|v| v.as_str().map(str::to_owned)).filter(|s| !s.is_empty())
        };

        Self {
            date_chicago: str_field("date_chicago")
                .filter(|s| is_day_key(s))
                .unwrap_or_else(|| current_day_key(cfg)),
            active_seconds: u64_field("active_seconds").unwrap_or(cfg.cap_seconds),
            // Reporting-only counter (never gates `block_status`), so an
            // unreadable value starts at zero rather than a fake total.
            weekly_active_seconds: u64_field("weekly_active_seconds").unwrap_or(0),
            courtesy_used: field("courtesy_used").and_then(|v| v.as_bool()).unwrap_or(true),
            // An extension we cannot verify is treated as not active.
            courtesy_expires_at: None,
            weekly_extension_week: str_field("weekly_extension_week")
                .filter(|s| is_week_key(s))
                .unwrap_or_else(|| current_week_key(cfg)),
            weekly_extension_used_seconds: u64_field("weekly_extension_used_seconds")
                .unwrap_or(cfg.weekly_extension_allowance_seconds),
            weekly_extension_expires_at: None,
            updated_at: unix_now(),
        }
    }

    /// Persist atomically (temp file + rename). Errors are logged but
    /// never propagated — the budget keeps running on the in-memory
    /// state even if disk writes fail.
    pub fn save(&mut self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                debug!("Could not create budget dir {}: {err}", parent.display());
                return;
            }
        }
        self.touch();
        let json = match serde_json::to_string_pretty(self) {
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

    /// Stamp the state as current. `updated_at` is a freshness marker for
    /// external consumers; the tick stamps it every second so the daemon's
    /// `/usage` payload stays fresh even on ticks whose disk write is
    /// skipped (nothing counted, heartbeat not due).
    pub fn touch(&mut self) {
        self.updated_at = unix_now();
    }

    /// True when every persisted field except the `updated_at` freshness
    /// stamp matches `other`. Used by the tick to skip redundant disk
    /// writes: any change here (counters, day/week keys, extensions) must
    /// hit disk immediately, while a pure freshness delta only needs the
    /// periodic heartbeat.
    pub fn same_persistent_state(&self, other: &Self) -> bool {
        self.date_chicago == other.date_chicago
            && self.active_seconds == other.active_seconds
            && self.weekly_active_seconds == other.weekly_active_seconds
            && self.courtesy_used == other.courtesy_used
            && self.courtesy_expires_at == other.courtesy_expires_at
            && self.weekly_extension_week == other.weekly_extension_week
            && self.weekly_extension_used_seconds == other.weekly_extension_used_seconds
            && self.weekly_extension_expires_at == other.weekly_extension_expires_at
    }

    /// Reset the day's accumulators only if the current day key has
    /// advanced **past** the one in `date_chicago`.
    ///
    /// Both key formats sort lexicographically (`%Y-%m-%d`, `%Y-W%W`), so
    /// a strict `>` comparison is a real "time moved forward" test. Plain
    /// inequality would treat a backwards system-clock change as a
    /// rollover and hand back the full daily budget on the next 1s tick.
    pub fn refresh_day_boundary(&mut self, cfg: &BudgetConfig) {
        let today = current_day_key(cfg);
        if today > self.date_chicago {
            debug!("Budget: day rollover {} -> {}, resetting counter", self.date_chicago, today);
            self.date_chicago = today;
            self.active_seconds = 0;
            self.courtesy_used = false;
            self.courtesy_expires_at = None;
        } else if today < self.date_chicago {
            warn!(
                "Budget: clock moved backwards ({} -> {}); keeping stored day key and counters",
                self.date_chicago, today
            );
        }
        self.refresh_week_boundary(cfg);
    }

    /// Reset the weekly extension allowance if the configured week key
    /// has advanced. Same strict-forward rule as the day boundary.
    pub fn refresh_week_boundary(&mut self, cfg: &BudgetConfig) {
        let week = current_week_key(cfg);
        if week > self.weekly_extension_week {
            debug!(
                "Budget: week rollover {} -> {}, resetting weekly extensions",
                self.weekly_extension_week, week
            );
            self.weekly_extension_week = week;
            self.weekly_active_seconds = 0;
            self.weekly_extension_used_seconds = 0;
            self.weekly_extension_expires_at = None;
        } else if week < self.weekly_extension_week {
            warn!(
                "Budget: clock moved backwards ({} -> {}); keeping stored week key and weekly \
                 counters",
                self.weekly_extension_week, week
            );
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
    day_key_of(now_in(cfg) - Duration::hours(cfg.sleep_end_hour_clamped() as i64))
}

/// Day key for an already-shifted local timestamp. Split out from
/// [`current_day_key`] so the lexicographic-monotonicity contract that
/// `refresh_day_boundary`'s strict `>` relies on is testable at arbitrary
/// dates instead of only "now".
fn day_key_of(shifted: chrono::DateTime<Tz>) -> String {
    shifted.format("%Y-%m-%d").to_string()
}

fn current_week_key(cfg: &BudgetConfig) -> String {
    week_key_of(now_in(cfg) - Duration::hours(cfg.sleep_end_hour_clamped() as i64))
}

/// Week key for an already-shifted local timestamp.
///
/// Uses the **ISO week-numbering year** (`IsoWeek::year()`), never the
/// calendar year: 2027-01-01 belongs to 2026-W53, and labelling it
/// `2027-W53` would make the following key (`2027-W01`) sort *backwards*.
/// `refresh_week_boundary` would then read a real week rollover as a clock
/// rollback and never refresh the allowance. Zero-padding the week keeps
/// `-W09` < `-W10`.
fn week_key_of(shifted: chrono::DateTime<Tz>) -> String {
    let week = shifted.iso_week();
    format!("{}-W{:02}", week.year(), week.week())
}

/// True when `key` has the exact `%Y-%m-%d` shape [`day_key_of`] emits.
/// Used to refuse a salvaged-but-nonsensical key during fail-closed load.
fn is_day_key(key: &str) -> bool {
    chrono::NaiveDate::parse_from_str(key, "%Y-%m-%d").is_ok()
}

/// True when `key` has the exact `<year>-W<2 digits>` shape
/// [`week_key_of`] emits, with a plausible ISO week number.
fn is_week_key(key: &str) -> bool {
    let Some((year, week)) = key.split_once("-W") else { return false };
    if year.len() < 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    week.len() == 2
        && week.bytes().all(|b| b.is_ascii_digit())
        && matches!(week.parse::<u8>(), Ok(1..=53))
}

fn in_sleep_window(cfg: &BudgetConfig) -> bool {
    let hour = now_in(cfg).hour() as u8;
    // Handle wraparound (start>end means "spans midnight") even though we
    // don't use that today — keeps the function honest.
    //
    // The raw configured hours are used here on purpose. Clamping to 0-23
    // exists to stop `NaiveTime::from_hms_opt` panicking, and this
    // comparison cannot panic — but clamping *can* shrink the window for
    // an out-of-range value. `sleep_end_hour = 250` clamps to 23 and drops
    // the 23:00 hour out of the block; `sleep_start_hour = 99` with
    // `sleep_end_hour = 23` clamps to `23..23`, an empty window, where the
    // raw comparison blocks 00:00-23:00. A typo must never buy unblocked
    // time, so enforcement reads the config as written.
    hour_in_window(cfg.sleep_start_hour, cfg.sleep_end_hour, hour)
}

/// Pure half of [`in_sleep_window`], split out so every hour of the day
/// can be tested without mocking the wall clock.
fn hour_in_window(start: u8, end: u8, hour: u8) -> bool {
    if start <= end { hour >= start && hour < end } else { hour >= start || hour < end }
}

fn seconds_until_sleep_end(cfg: &BudgetConfig) -> u64 {
    let tz = resolve_tz(&cfg.timezone);
    let now = chrono::Utc::now().with_timezone(&tz);
    seconds_until_hour(now, tz, cfg.sleep_end_hour_clamped())
}

fn seconds_until_next_day(cfg: &BudgetConfig) -> u64 {
    let tz = resolve_tz(&cfg.timezone);
    let now = chrono::Utc::now().with_timezone(&tz);
    seconds_until_hour(now, tz, cfg.sleep_end_hour_clamped())
}

fn seconds_until_hour(now: chrono::DateTime<Tz>, tz: Tz, hour: u8) -> u64 {
    // `hour` arrives pre-clamped to 0-23 via `BudgetConfig::*_clamped()`;
    // fall back to midnight rather than panicking the event loop (this is
    // reached from the 1s tick and from the daemon's `GET /usage`).
    let sleep_end = NaiveTime::from_hms_opt(u32::from(hour.min(23)), 0, 0).unwrap_or_default();
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
    fn backwards_clock_does_not_reset_day_or_week_counters() {
        let cfg = BudgetConfig::default();
        let mut budget = Budget {
            // Day/week keys far in the future: "now" is behind them, which
            // is what a backwards system-clock change looks like.
            date_chicago: "9999-12-31".to_string(),
            active_seconds: 9_000,
            weekly_active_seconds: 20_000,
            courtesy_used: true,
            courtesy_expires_at: None,
            weekly_extension_week: "9999-W52".to_string(),
            weekly_extension_used_seconds: 7_200,
            weekly_extension_expires_at: None,
            updated_at: 0,
        };
        budget.refresh_day_boundary(&cfg);

        assert_eq!(budget.date_chicago, "9999-12-31", "stored day key must survive");
        assert_eq!(budget.active_seconds, 9_000, "backwards clock must not refund the day");
        assert!(budget.courtesy_used, "backwards clock must not refund the courtesy");
        assert_eq!(budget.weekly_extension_week, "9999-W52", "stored week key must survive");
        assert_eq!(budget.weekly_extension_used_seconds, 7_200, "weekly allowance must survive");
        assert_eq!(budget.weekly_active_seconds, 20_000);
    }

    #[test]
    fn forward_day_rollover_still_resets() {
        let cfg = BudgetConfig::default();
        let mut budget = Budget {
            date_chicago: "1970-01-01".to_string(),
            active_seconds: 9_000,
            courtesy_used: true,
            weekly_extension_week: "1970-W01".to_string(),
            weekly_extension_used_seconds: 7_200,
            ..Budget::default()
        };
        budget.refresh_day_boundary(&cfg);

        assert_eq!(budget.date_chicago, current_day_key(&cfg));
        assert_eq!(budget.active_seconds, 0);
        assert!(!budget.courtesy_used);
        assert_eq!(budget.weekly_extension_used_seconds, 0);
    }

    #[test]
    fn fail_closed_assumes_spent_when_counters_are_unreadable() {
        let cfg = BudgetConfig::default();
        let budget = Budget::fail_closed(&cfg, "{not json");

        assert_eq!(budget.active_seconds, cfg.cap_seconds, "must not hand back a fresh day");
        assert!(budget.courtesy_used, "unverifiable courtesy must count as spent");
        assert_eq!(budget.weekly_extension_used_seconds, cfg.weekly_extension_allowance_seconds);
        assert!(budget.block_status(&cfg).is_some() || in_sleep_window(&cfg));
    }

    #[test]
    fn fail_closed_salvages_legible_counters() {
        let cfg = BudgetConfig::default();
        // Valid JSON that fails strict `Budget` deserialization (missing
        // required fields) but still exposes the day's counter.
        let raw = r#"{"date_chicago":"2026-05-22","active_seconds":1234}"#;
        let budget = Budget::fail_closed(&cfg, raw);

        assert_eq!(budget.date_chicago, "2026-05-22");
        assert_eq!(budget.active_seconds, 1_234, "legible counter must be kept verbatim");
        assert!(budget.courtesy_used, "absent courtesy flag still fails closed");
    }

    /// A damaged file whose day key is legible JSON but not a date used to
    /// be trusted verbatim. Because `load_or_default` calls
    /// `refresh_day_boundary` right after `fail_closed`, and every real
    /// `%Y-%m-%d` key sorts above strings like `"0"` or `"!!"`, that
    /// turned the fail-closed path into a full day refund.
    #[test]
    fn fail_closed_rejects_a_day_key_that_is_not_a_date() {
        let cfg = BudgetConfig::default();
        for bogus in ["0", "!!", "corrupt", "2026-13-99", ""] {
            let raw = format!(r#"{{"date_chicago":{bogus:?},"courtesy_used":false}}"#);
            let mut budget = Budget::fail_closed(&cfg, &raw);
            assert_eq!(
                budget.date_chicago,
                current_day_key(&cfg),
                "bogus day key {bogus:?} must fall back to today, not be trusted"
            );

            // The rollover that follows in `load_or_default` must not fire.
            budget.refresh_day_boundary(&cfg);
            assert_eq!(
                budget.active_seconds, cfg.cap_seconds,
                "corruption must not refund the day via a bogus key: {bogus:?}"
            );
        }
    }

    /// Same hole on the weekly allowance.
    #[test]
    fn fail_closed_rejects_a_week_key_that_is_not_a_week() {
        let cfg = BudgetConfig::default();
        for bogus in ["0", "2026-W", "2026-W0", "2026-W99", "2026-WXX", "wat"] {
            let raw = format!(r#"{{"weekly_extension_week":{bogus:?}}}"#);
            let mut budget = Budget::fail_closed(&cfg, &raw);
            assert_eq!(budget.weekly_extension_week, current_week_key(&cfg), "bogus: {bogus:?}");

            budget.refresh_week_boundary(&cfg);
            assert_eq!(
                budget.weekly_extension_used_seconds, cfg.weekly_extension_allowance_seconds,
                "corruption must not refund the weekly allowance: {bogus:?}"
            );
        }
    }

    /// A legible, genuinely-shaped past key is still honoured — a damaged
    /// file written yesterday must still roll over normally.
    #[test]
    fn fail_closed_still_honours_a_well_formed_past_day_key() {
        let cfg = BudgetConfig::default();
        let raw = r#"{"date_chicago":"2000-01-02"}"#;
        let mut budget = Budget::fail_closed(&cfg, raw);
        assert_eq!(budget.date_chicago, "2000-01-02");

        budget.refresh_day_boundary(&cfg);
        assert_eq!(budget.date_chicago, current_day_key(&cfg), "a real past day must roll over");
        assert_eq!(budget.active_seconds, 0);
    }

    /// The strict `>` in `refresh_{day,week}_boundary` is only a
    /// "time moved forward" test while both key formats sort
    /// lexicographically. Walk several year boundaries and assert neither
    /// key ever goes backwards — in particular that the week key uses the
    /// ISO week-numbering year, so 2026-12-31 → `2026-W53`,
    /// 2027-01-01 → `2026-W53`, 2027-01-04 → `2027-W01`.
    #[test]
    fn day_and_week_keys_are_lexicographically_monotonic_across_year_boundaries() {
        let tz = resolve_tz("America/Chicago");
        let mut date = chrono::NaiveDate::from_ymd_opt(2024, 12, 1).unwrap();
        let last = chrono::NaiveDate::from_ymd_opt(2031, 2, 1).unwrap();
        let (mut prev_day, mut prev_week) = (String::new(), String::new());

        while date <= last {
            let at = tz.from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap()).unwrap();
            let (day, week) = (day_key_of(at), week_key_of(at));
            assert!(day > prev_day, "day key went backwards: {prev_day} -> {day}");
            assert!(week >= prev_week, "week key went backwards: {prev_week} -> {week}");
            (prev_day, prev_week) = (day, week);
            date = date.succ_opt().unwrap();
        }

        let key_at = |y, m, d| {
            let naive = chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(12, 0, 0);
            week_key_of(tz.from_local_datetime(&naive.unwrap()).unwrap())
        };
        assert_eq!(key_at(2026, 12, 31), "2026-W53");
        assert_eq!(key_at(2027, 1, 1), "2026-W53", "ISO week-year, not calendar year");
        assert_eq!(key_at(2027, 1, 4), "2027-W01");
        assert!(key_at(2026, 12, 31) < key_at(2027, 1, 4));
    }

    /// Clamping an out-of-range hour exists to stop `NaiveTime` panicking;
    /// it must never shrink the block. Both cases below lost enforcement
    /// time when `in_sleep_window` compared clamped hours.
    #[test]
    fn out_of_range_sleep_hours_never_shrink_the_window() {
        let window =
            |start: u8, end: u8| (0u8..24).filter(|h| hour_in_window(start, end, *h)).count();

        // `sleep_end_hour = 250` clamped to 23 used to drop the 23:00 hour.
        assert_eq!(window(2, 250), 22, "02:00-24:00 must stay 22 hours");
        // `sleep_start_hour = 99` + `sleep_end_hour = 23` clamped to a
        // start==end pair, i.e. an empty window where 23 hours were blocked.
        assert_eq!(window(99, 23), 23, "00:00-23:00 must stay 23 hours");
        // Valid configs are untouched.
        assert_eq!(window(2, 8), 6);
        assert_eq!(window(22, 6), 8);
    }

    #[test]
    fn out_of_range_sleep_hour_does_not_panic() {
        let mut cfg = BudgetConfig::default();
        cfg.sleep_end_hour = 250;
        cfg.sleep_start_hour = 99;
        assert_eq!(cfg.sleep_end_hour_clamped(), 23);
        assert_eq!(cfg.sleep_start_hour_clamped(), 23);
        // Both of these used to panic the event loop via `expect`.
        let _ = seconds_until_sleep_end(&cfg);
        let _ = in_sleep_window(&cfg);
    }

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
        assert_eq!(
            seconds_until_hour(now, tz, cfg.sleep_end_hour_clamped()),
            10 * 60 * 60 + 30 * 60
        );
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
