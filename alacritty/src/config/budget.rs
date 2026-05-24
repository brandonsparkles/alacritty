//! Daily-usage budget configuration. macOS only.
//!
//! Read by `crate::budget` and the hide-when-inactive hook in the event
//! loop. Every field is opt-in via `alacritty.toml`; defaults reflect the
//! commitment-device design (3 h cap, 02:00–08:00 sleep window in Chicago
//! local time, hide when the app is not front-most).
//!
//! Sample `~/.config/alacritty/alacritty.toml`:
//!
//! ```toml
//! [budget]
//! enabled = true
//! cap_seconds = 10800           # 3 hours of focused time per day
//! sleep_start_hour = 2          # 02:00 local — hard cutoff
//! sleep_end_hour = 8            # 08:00 local — unlock + daily reset
//! timezone = "America/Chicago"
//! hide_when_inactive = true     # Cmd-Tab away → all windows hide
//! allow_courtesy = true         # one courtesy extension per day
//! courtesy_seconds = 900        # 15 minutes
//! allow_weekly_extensions = true
//! weekly_extension_seconds = 3600
//! weekly_extension_allowance_seconds = 21600
//! ```

use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

/// Default daily cap. 3 hours.
pub const DEFAULT_CAP_SECONDS: u64 = 3 * 60 * 60;

#[derive(ConfigDeserialize, Serialize, Clone, PartialEq, Debug)]
pub struct BudgetConfig {
    /// Master switch. When false, no counter ticks and no lockout fires —
    /// the budget machinery is entirely passive.
    pub enabled: bool,

    /// Daily active-time cap, in seconds. Defaults to 10800 (3 hours).
    pub cap_seconds: u64,

    /// Hour of day (0–23) at which the sleep-window block engages.
    pub sleep_start_hour: u8,

    /// Hour of day (0–23) at which the sleep-window block lifts. Between
    /// `sleep_start_hour` and `sleep_end_hour`, the app is hard-locked
    /// regardless of remaining budget. The daily counter also resets at
    /// this hour.
    pub sleep_end_hour: u8,

    /// IANA timezone the sleep window and day boundaries are anchored to.
    /// e.g. `"America/Chicago"`. Invalid values fall back to Chicago.
    pub timezone: String,

    /// When true, the app hides all its windows after losing active
    /// state (Cmd-Tab away, click another window, etc.). The reverse of
    /// default macOS behavior — designed as a commitment device that
    /// prevents passive peripheral-vision use.
    pub hide_when_inactive: bool,

    /// Grace period in seconds before `hide_when_inactive` actually fires.
    /// During this window, Alacritty stays visible so you can reference
    /// code/output while working in another app. The active-time counter
    /// continues to advance during the grace window (you're still using
    /// it, just not focused on it).
    ///
    /// `0` = immediate hide. Default `300` = 5 minutes.
    /// Ignored when `hide_when_inactive` is false.
    pub background_grace_seconds: u64,

    /// When true, allow one courtesy extension per day.
    ///
    /// Defaults to true, but it is one-shot per day and still unavailable
    /// during the sleep window.
    pub allow_courtesy: bool,

    /// Size of the once-per-day courtesy extension, in seconds.
    ///
    /// Default `900` = 15 minutes. Set to `0` to make `allow_courtesy`
    /// inert without removing the key.
    pub courtesy_seconds: u64,

    /// When true, allow redeeming weekly extension time after the daily
    /// budget is exhausted.
    pub allow_weekly_extensions: bool,

    /// Size of each weekly extension redemption, in seconds.
    ///
    /// Default `3600` = one hour.
    pub weekly_extension_seconds: u64,

    /// Total weekly extension allowance, in seconds.
    ///
    /// Default `21600` = six hours.
    pub weekly_extension_allowance_seconds: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cap_seconds: DEFAULT_CAP_SECONDS,
            sleep_start_hour: 2,
            sleep_end_hour: 8,
            timezone: "America/Chicago".to_string(),
            hide_when_inactive: true,
            background_grace_seconds: 300,
            allow_courtesy: true,
            courtesy_seconds: 15 * 60,
            allow_weekly_extensions: true,
            weekly_extension_seconds: 60 * 60,
            weekly_extension_allowance_seconds: 6 * 60 * 60,
        }
    }
}
