//! Persisted application settings.
//!
//! Stored as JSON in the app's own data directory, deliberately NOT in
//! `cswap`'s `settings.json`. Sharing that file would mean two processes
//! writing the same config with no coordination, and a partial write from
//! either could leave the CLI unable to start. Interop is a promise about
//! *credential* state, not about config.
//!
//! Key names and defaults deliberately mirror `cswap`'s (`autoswitch.threshold`
//! and friends) so a user reading both tools sees the same vocabulary and the
//! same numbers mean the same things.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How the next account is chosen when auto-switching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Jump to whichever account has the most headroom. `cswap`'s "best".
    #[default]
    MostHeadroom,
    /// Rotate to the next account, skipping any at their limit.
    NextAvailable,
    /// Drain the soonest-resetting account first.
    ConsumeFirst,
}

impl<'de> Deserialize<'de> for Strategy {
    /// Tolerant on purpose: an unrecognised value in a settings file must
    /// cost one field, not the entire file. Also accepts the `cswap` CLI's
    /// own spelling (`best`) so a value copied from its config means what the
    /// user expects.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "most-headroom" | "best" => Self::MostHeadroom,
            "next-available" => Self::NextAvailable,
            "consume-first" => Self::ConsumeFirst,
            _ => Self::default(),
        })
    }
}

/// Colour theme for the app windows.
///
/// Distinct from the tray icon's theme, which follows the OS taskbar rather
/// than this preference — the icon sits in the taskbar, not in our window, so
/// it has to read against whatever is behind it. See `tray::AmbientTheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the OS, live — the frontend removes `data-theme` so the
    /// `prefers-color-scheme` media query governs.
    #[default]
    System,
    Day,
    Night,
}

impl<'de> Deserialize<'de> for Theme {
    /// Tolerant, for the same reason as [`Strategy`]: an unrecognised value in
    /// a settings file must cost one field, never the whole file. Also accepts
    /// the light/dark spelling, since that is what `data-theme` uses on the
    /// frontend and what the `cswap` CLI calls its own theme setting.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "day" | "light" => Self::Day,
            "night" | "dark" => Self::Night,
            "system" | "auto" => Self::System,
            _ => Self::default(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Off by default, and that is a product decision, not an oversight.
    /// Software that starts moving credentials before being asked is not
    /// trustworthy. The user opts in.
    pub auto_switch_enabled: bool,

    /// Utilisation percentage that arms a switch. `cswap` default is 90.
    pub threshold: u8,
    /// Minimum gap between usage checks, not a fixed period — the adaptive
    /// poller in `poller.rs` may check less often than this (it backs off
    /// after a 429 and decays toward its own ceiling when nothing is
    /// moving), but never more often. Clamped in [`Settings::sanitised`] to
    /// `180..3600`; see that comment for where 180 comes from.
    pub interval_seconds: u64,
    /// Minimum gap between switches, to stop flip-flopping.
    pub cooldown_seconds: u64,
    /// An account must drop this far below the threshold before it is eligible
    /// again, so a value hovering at the line does not oscillate.
    pub hysteresis_pct: u8,
    /// Consecutive failures before an account is treated as unhealthy. One
    /// network blip must not eject an account.
    pub unhealthy_ticks: u8,
    pub strategy: Strategy,

    /// Seconds of warning before an armed switch fires. Zero switches
    /// immediately. Silently moving credentials mid-task is worse than the
    /// limit it avoids, so the default is a real pause.
    pub grace_seconds: u64,

    pub notify_on_switch: bool,
    pub notify_on_exhausted: bool,
    pub notify_on_expiry: bool,

    pub start_at_login: bool,

    /// Colour theme for the app windows.
    pub theme: Theme,

    /// Days of raw history kept before downsampling to daily rollups.
    pub history_retention_days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_switch_enabled: false,
            threshold: 90,
            // 5 minutes. Comfortably above the 180s measured floor (see
            // `sanitised` below) rather than sitting right on it, and matches
            // the default refresh cadence of the Claude Code status-line
            // tools that poll this same endpoint, so the number means the
            // same thing here as it does elsewhere in the ecosystem. The old
            // default of 60 sat below that floor and was silently overridden
            // to 180 by the poller — exactly the dishonest-control shape
            // `sanitised` now closes off.
            interval_seconds: 300,
            cooldown_seconds: 300,
            hysteresis_pct: 10,
            unhealthy_ticks: 3,
            strategy: Strategy::default(),
            grace_seconds: 60,
            notify_on_switch: true,
            notify_on_exhausted: true,
            notify_on_expiry: false,
            start_at_login: false,
            theme: Theme::default(),
            history_retention_days: 14,
        }
    }
}

impl Settings {
    /// Clamp anything a hand-edited file could get wrong.
    ///
    /// Applied on load rather than rejecting the file: a settings file with one
    /// silly number should not stop the app from starting.
    pub fn sanitised(mut self) -> Self {
        self.threshold = self.threshold.clamp(50, 99);
        // Floor of 180s matches `poller::poll_policy::MIN_INTERVAL_S` exactly
        // — not an arbitrary round number. It comes from the real usage
        // endpoint's behaviour (ported from `claude_swap/poll_policy.py`,
        // which derived it empirically): the endpoint enforces a rolling
        // ~60-minute budget of roughly 28-30 requests per access token — a
        // fixed window, not a refilling bucket — and the endpoint owner's own
        // stated target is an average of at most ~1 request per 3 minutes per
        // token. That is 180s. The poller already enforces this floor by
        // silently overriding anything lower (`interval_s.max(MIN_INTERVAL_S)`
        // in `poller.rs`), so accepting a smaller value here would let this
        // control accept and "save" a number that can never actually take
        // effect — precisely the lie this clamp exists to close off. Do not
        // lower this without first re-deriving the budget against the real
        // endpoint; a live run has already produced a 429 storm from
        // over-eager polling once.
        self.interval_seconds = self.interval_seconds.clamp(180, 3600);
        self.cooldown_seconds = self.cooldown_seconds.min(86_400);
        self.hysteresis_pct = self.hysteresis_pct.min(50);
        self.unhealthy_ticks = self.unhealthy_ticks.clamp(1, 20);
        self.grace_seconds = self.grace_seconds.min(3600);
        self.history_retention_days = self.history_retention_days.clamp(1, 3650);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// `<dir>/settings.json`.
pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

/// Load settings, falling back to defaults.
///
/// A missing file is normal (first run). A corrupt file returns defaults with a
/// warning rather than failing: being unable to start because one config value
/// is malformed is a worse outcome than starting with known-good defaults.
pub fn load(dir: &Path) -> Settings {
    let path = settings_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(s) => s.sanitised(),
            Err(e) => {
                log::warn!("settings at {} are unreadable ({e}); using defaults", path.display());
                Settings::default()
            }
        },
        Err(_) => Settings::default(),
    }
}

/// Save settings atomically (temp file then rename), so a crash mid-write
/// cannot leave a truncated config that fails to parse next launch.
pub fn save(dir: &Path, settings: &Settings) -> Result<(), SettingsError> {
    std::fs::create_dir_all(dir)?;
    let path = settings_path(dir);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(&settings.clone().sanitised())?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_switch_is_off_by_default() {
        // Load-bearing product decision: never move credentials unasked.
        assert!(!Settings::default().auto_switch_enabled);
    }

    #[test]
    fn grace_period_defaults_to_a_real_pause() {
        assert_eq!(Settings::default().grace_seconds, 60);
    }

    #[test]
    fn absurd_values_are_clamped_not_rejected() {
        let wild = Settings {
            threshold: 250,
            interval_seconds: 1,
            hysteresis_pct: 200,
            unhealthy_ticks: 0,
            grace_seconds: 999_999,
            history_retention_days: -5,
            ..Default::default()
        }
        .sanitised();

        assert_eq!(wild.threshold, 99);
        // 180, not 15: a value the poller would silently override must not
        // be accepted as "saved" — see the clamp's own comment.
        assert_eq!(wild.interval_seconds, 180);
        assert_eq!(wild.hysteresis_pct, 50);
        assert_eq!(wild.unhealthy_ticks, 1);
        assert_eq!(wild.grace_seconds, 3600);
        assert_eq!(wild.history_retention_days, 1);
    }

    #[test]
    fn default_interval_matches_the_ecosystem_s_5_minute_convention() {
        // The old default (60s) sat below the poller's real floor and was
        // silently raised to 180 — a fresh install shipped one of the
        // dishonest values this whole task exists to remove. 300s is
        // comfortably above the 180s floor and matches what Claude Code
        // status-line tools default to against this same endpoint.
        assert_eq!(Settings::default().interval_seconds, 300);
    }

    #[test]
    fn a_sub_floor_interval_is_clamped_up_to_the_measured_floor() {
        let s = Settings { interval_seconds: 30, ..Default::default() }.sanitised();
        assert_eq!(s.interval_seconds, 180);
    }

    #[test]
    fn the_floor_itself_is_accepted_unchanged() {
        let s = Settings { interval_seconds: 180, ..Default::default() }.sanitised();
        assert_eq!(s.interval_seconds, 180);
    }

    #[test]
    fn an_interval_above_the_ceiling_is_clamped_down_to_it() {
        let s = Settings { interval_seconds: 999_999, ..Default::default() }.sanitised();
        assert_eq!(s.interval_seconds, 3600);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        s.auto_switch_enabled = true;
        s.threshold = 85;
        s.strategy = Strategy::ConsumeFirst;

        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()), s);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Settings::default());
    }

    #[test]
    fn corrupt_file_yields_defaults_rather_than_failing_to_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(settings_path(dir.path()), "{ not json at all").unwrap();
        assert_eq!(load(dir.path()), Settings::default());
    }

    #[test]
    fn an_old_file_with_the_removed_storage_mode_key_still_loads_with_its_other_fields_intact() {
        // `storageMode` used to choose between this app's own vault and the
        // shared `cswap` directory; that choice no longer exists (the vault
        // is now always ours — see `paths::backup_root`), so the field is
        // gone from `Settings` entirely. A settings file a previous build
        // wrote still has this key on disk, though, and loading it must not
        // regress to the old failure mode where an unparseable/unknown field
        // silently discarded every *other* setting in the file — it should
        // simply be ignored, the same as any other key this build doesn't
        // recognise (see `unknown_keys_do_not_break_loading` below).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"threshold": 72, "graceSeconds": 15, "storageMode": "compatible"}"#,
        )
        .unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.threshold, 72);
        assert_eq!(loaded.grace_seconds, 15);
    }

    #[test]
    fn an_unrecognised_enum_costs_one_field_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"threshold": 66, "strategy": "invented-by-a-future-build"}"#,
        )
        .unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.strategy, Strategy::default());
        assert_eq!(loaded.threshold, 66, "the rest of the file must survive");
    }

    #[test]
    fn the_cli_spelling_of_a_strategy_is_understood() {
        // `cswap` calls this "best"; a value copied from its config should
        // mean what the user expects rather than silently reverting.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(settings_path(dir.path()), r#"{"strategy": "best"}"#).unwrap();
        assert_eq!(load(dir.path()).strategy, Strategy::MostHeadroom);
    }

    #[test]
    fn unknown_keys_do_not_break_loading() {
        // A settings file written by a newer build must not brick an older one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"threshold": 80, "somethingFromTheFuture": true}"#,
        )
        .unwrap();
        assert_eq!(load(dir.path()).threshold, 80);
    }
}
