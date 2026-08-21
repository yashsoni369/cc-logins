//! Persisted application settings.
//!
//! Stored as JSON in the app's own data directory, never in another tool's
//! settings file. Sharing one would mean two processes writing the same
//! config with no coordination, and a partial write from either could leave
//! the other unable to start.
//!
//! Key names and defaults deliberately mirror `cswap`'s (`autoswitch.threshold`
//! and friends) so a user reading both tools sees the same vocabulary and the
//! same numbers mean the same things.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::runtime::RuntimePolicy;

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

/// How clock times are rendered across the app.
///
/// Persisted rather than read from the OS at each launch: the frontend shows
/// times in several places at once (history, next-reset, tray tooltips) and
/// they have to agree, so one stored answer beats each view guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ClockFormat {
    /// Follow the OS locale, live — the frontend formats without an explicit
    /// `hour12`, so the user's regional setting governs.
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "12h")]
    H12,
    #[serde(rename = "24h")]
    H24,
}

impl<'de> Deserialize<'de> for ClockFormat {
    /// Tolerant, for the same reason as [`Theme`]: an unrecognised value in a
    /// settings file must cost one field, never the whole file. The spellings
    /// are the ones a human hand-editing the file or another tool writing it
    /// would plausibly reach for, including the `hourCycle` names (`h12`/`h23`)
    /// the frontend's own formatter uses.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "12h" | "12" | "12-hour" | "h12" | "ampm" => Self::H12,
            "24h" | "24" | "24-hour" | "h23" | "military" => Self::H24,
            "system" | "auto" | "locale" => Self::System,
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

    /// A persisted global pause for automatic switching. Polling, history,
    /// and manual switching continue while this deadline is in the future.
    #[serde(default)]
    pub auto_switch_paused_until: Option<chrono::DateTime<chrono::Utc>>,

    /// Utilisation percentage that arms a switch. `cswap` default is 90.
    pub threshold: u8,
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

    /// Whether the app may ask GitHub for a newer release on its own.
    ///
    /// On by default: an update path nobody discovers is not an update path,
    /// and every install otherwise stays on the version it was downloaded at.
    /// This is the only setting that permits an outbound request to anything
    /// other than Anthropic, so it is named in the README rather than buried,
    /// and turning it off leaves the manual check in Settings -> About working.
    pub auto_check_updates: bool,

    /// Colour theme for the app windows.
    pub theme: Theme,

    /// 12- or 24-hour clocks, so every view agrees on how a time reads.
    pub clock_format: ClockFormat,

    /// Days of raw history kept before downsampling to daily rollups.
    pub history_retention_days: i64,

    /// Full path to the `claude` binary, when auto-discovery ([`crate::claude_cli`])
    /// cannot find it and setting an environment variable is not viable — a
    /// Dock/Finder-launched app never sees a shell's exported variables.
    ///
    /// Deliberately **not** part of [`RuntimePolicy`]: the background poller
    /// never launches `claude`, only interactive login does, so this field has
    /// nothing to contribute to the policy the poller reads.
    pub claude_binary_path: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_switch_enabled: false,
            auto_switch_paused_until: None,
            threshold: 90,
            cooldown_seconds: 300,
            hysteresis_pct: 10,
            unhealthy_ticks: 3,
            strategy: Strategy::default(),
            grace_seconds: 60,
            notify_on_switch: true,
            notify_on_exhausted: true,
            notify_on_expiry: false,
            start_at_login: false,
            auto_check_updates: true,
            theme: Theme::default(),
            clock_format: ClockFormat::default(),
            history_retention_days: 14,
            claude_binary_path: None,
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
        self.cooldown_seconds = self.cooldown_seconds.min(86_400);
        self.hysteresis_pct = self.hysteresis_pct.min(50);
        self.unhealthy_ticks = self.unhealthy_ticks.clamp(1, 20);
        self.grace_seconds = self.grace_seconds.min(3600);
        self.history_retention_days = self.history_retention_days.clamp(1, 3650);
        // Trim and empty-to-None only — nothing path-shaped. Whether a path is
        // executable is time-varying (wrong at save time can be right by the
        // time login runs), so this clamps stray whitespace, never discards a
        // value on the strength of a stat() done here.
        self.claude_binary_path = self
            .claude_binary_path
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        self
    }
}

/// A complete, revisioned view of the persisted settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    pub revision: u64,
    pub settings: Settings,
}

/// A partial settings update. `None` means the caller omitted a field.
///
/// The pause field is intentionally nested: an omitted value preserves the
/// current deadline, while an explicit JSON `null` clears it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsPatch {
    pub auto_switch_enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    pub auto_switch_paused_until: Option<Option<DateTime<Utc>>>,
    pub threshold: Option<u8>,
    pub cooldown_seconds: Option<u64>,
    pub hysteresis_pct: Option<u8>,
    pub unhealthy_ticks: Option<u8>,
    pub strategy: Option<Strategy>,
    pub grace_seconds: Option<u64>,
    pub notify_on_switch: Option<bool>,
    pub notify_on_exhausted: Option<bool>,
    pub notify_on_expiry: Option<bool>,
    pub start_at_login: Option<bool>,
    pub auto_check_updates: Option<bool>,
    pub theme: Option<Theme>,
    pub clock_format: Option<ClockFormat>,
    pub history_retention_days: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    pub claude_binary_path: Option<Option<String>>,
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsUpdateError {
    #[error(
        "settings revision conflict: expected revision {expected_revision}, current revision is {actual_revision}"
    )]
    Conflict {
        expected_revision: u64,
        actual_revision: u64,
    },
    #[error("failed to persist settings: {0}")]
    Persist(#[from] SettingsError),
    #[error("pause duration is outside the supported date range")]
    InvalidDuration,
}

struct SettingsState {
    revision: u64,
    settings: Settings,
}

/// The single mutation path for settings, persisted state, and daemon policy.
///
/// Updates hold the state lock through persistence so concurrent callers
/// cannot publish or save revisions out of order. The watch channel is only
/// advanced after the atomic save succeeds.
pub struct SettingsStore {
    data_dir: PathBuf,
    state: Mutex<SettingsState>,
    policy_tx: watch::Sender<RuntimePolicy>,
}

impl SettingsStore {
    pub fn new(data_dir: PathBuf, now: DateTime<Utc>) -> Self {
        let settings = load(&data_dir);
        let revision = 0;
        let (policy_tx, _) = watch::channel(RuntimePolicy::from_settings(revision, &settings, now));
        Self {
            data_dir,
            state: Mutex::new(SettingsState { revision, settings }),
            policy_tx,
        }
    }

    pub fn snapshot(&self) -> SettingsSnapshot {
        let state = self.lock_state();
        SettingsSnapshot {
            revision: state.revision,
            settings: state.settings.clone(),
        }
    }

    pub fn subscribe_policy(&self) -> watch::Receiver<RuntimePolicy> {
        self.policy_tx.subscribe()
    }

    pub fn update(
        &self,
        expected_revision: u64,
        patch: SettingsPatch,
        now: DateTime<Utc>,
    ) -> Result<SettingsSnapshot, SettingsUpdateError> {
        let mut state = self.lock_state();
        if state.revision != expected_revision {
            return Err(SettingsUpdateError::Conflict {
                expected_revision,
                actual_revision: state.revision,
            });
        }
        self.commit_locked(&mut state, patch, now)
    }

    pub fn snooze(
        &self,
        duration: Duration,
        now: DateTime<Utc>,
    ) -> Result<SettingsSnapshot, SettingsUpdateError> {
        let offset =
            ChronoDuration::from_std(duration).map_err(|_| SettingsUpdateError::InvalidDuration)?;
        let until = now
            .checked_add_signed(offset)
            .ok_or(SettingsUpdateError::InvalidDuration)?;
        let mut state = self.lock_state();
        self.commit_locked(
            &mut state,
            SettingsPatch {
                auto_switch_paused_until: Some(Some(until)),
                ..SettingsPatch::default()
            },
            now,
        )
    }

    pub fn resume(&self, now: DateTime<Utc>) -> Result<SettingsSnapshot, SettingsUpdateError> {
        let mut state = self.lock_state();
        self.commit_locked(
            &mut state,
            SettingsPatch {
                auto_switch_paused_until: Some(None),
                ..SettingsPatch::default()
            },
            now,
        )
    }

    fn lock_state(&self) -> MutexGuard<'_, SettingsState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn commit_locked(
        &self,
        state: &mut SettingsState,
        patch: SettingsPatch,
        now: DateTime<Utc>,
    ) -> Result<SettingsSnapshot, SettingsUpdateError> {
        let mut settings = state.settings.clone();
        apply_patch(&mut settings, patch);
        if settings
            .auto_switch_paused_until
            .is_some_and(|until| until <= now)
        {
            settings.auto_switch_paused_until = None;
        }
        settings = settings.sanitised();
        let revision = state.revision.saturating_add(1);

        save(&self.data_dir, &settings)?;

        state.revision = revision;
        state.settings = settings.clone();
        self.policy_tx
            .send_replace(RuntimePolicy::from_settings(revision, &settings, now));

        Ok(SettingsSnapshot { revision, settings })
    }
}

fn apply_patch(settings: &mut Settings, patch: SettingsPatch) {
    if let Some(value) = patch.auto_switch_enabled {
        settings.auto_switch_enabled = value;
    }
    if let Some(value) = patch.auto_switch_paused_until {
        settings.auto_switch_paused_until = value;
    }
    if let Some(value) = patch.threshold {
        settings.threshold = value;
    }
    if let Some(value) = patch.cooldown_seconds {
        settings.cooldown_seconds = value;
    }
    if let Some(value) = patch.hysteresis_pct {
        settings.hysteresis_pct = value;
    }
    if let Some(value) = patch.unhealthy_ticks {
        settings.unhealthy_ticks = value;
    }
    if let Some(value) = patch.strategy {
        settings.strategy = value;
    }
    if let Some(value) = patch.grace_seconds {
        settings.grace_seconds = value;
    }
    if let Some(value) = patch.notify_on_switch {
        settings.notify_on_switch = value;
    }
    if let Some(value) = patch.notify_on_exhausted {
        settings.notify_on_exhausted = value;
    }
    if let Some(value) = patch.notify_on_expiry {
        settings.notify_on_expiry = value;
    }
    if let Some(value) = patch.start_at_login {
        settings.start_at_login = value;
    }
    if let Some(value) = patch.auto_check_updates {
        settings.auto_check_updates = value;
    }
    if let Some(value) = patch.theme {
        settings.theme = value;
    }
    if let Some(value) = patch.clock_format {
        settings.clock_format = value;
    }
    if let Some(value) = patch.history_retention_days {
        settings.history_retention_days = value;
    }
    if let Some(value) = patch.claude_binary_path {
        settings.claude_binary_path = value;
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
                log::warn!(
                    "settings at {} are unreadable ({e}); using defaults",
                    path.display()
                );
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
    use std::time::Duration;

    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

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
    fn auto_switch_pause_is_absent_by_default() {
        assert_eq!(Settings::default().auto_switch_paused_until, None);
    }

    #[test]
    fn auto_switch_pause_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let until = chrono::DateTime::parse_from_rfc3339("2026-07-28T13:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let settings = Settings {
            auto_switch_paused_until: Some(until),
            ..Settings::default()
        };

        save(dir.path(), &settings).unwrap();

        assert_eq!(load(dir.path()).auto_switch_paused_until, Some(until));
    }

    #[test]
    fn absurd_values_are_clamped_not_rejected() {
        let wild = Settings {
            threshold: 250,
            hysteresis_pct: 200,
            unhealthy_ticks: 0,
            grace_seconds: 999_999,
            history_retention_days: -5,
            ..Default::default()
        }
        .sanitised();

        assert_eq!(wild.threshold, 99);
        assert_eq!(wild.hysteresis_pct, 50);
        assert_eq!(wild.unhealthy_ticks, 1);
        assert_eq!(wild.grace_seconds, 3600);
        assert_eq!(wild.history_retention_days, 1);
    }

    /// The poll cadence used to be a user-facing setting. It is now fixed in
    /// `poller::poll_policy`, so a settings file written by an older build
    /// still carries an `intervalSeconds` key. Dropping the field must not
    /// cost the user every other setting in the same file — this is the same
    /// failure mode that renaming `StorageMode` once caused.
    #[test]
    fn a_settings_file_from_before_the_interval_was_fixed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"intervalSeconds":600,"threshold":77,"autoSwitchEnabled":true,"graceSeconds":300}"#,
        )
        .unwrap();

        let loaded = load(dir.path());

        // The retired key is ignored, and everything beside it survives.
        assert_eq!(loaded.threshold, 77);
        assert!(loaded.auto_switch_enabled);
        assert_eq!(loaded.grace_seconds, 300);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            auto_switch_enabled: true,
            threshold: 85,
            strategy: Strategy::ConsumeFirst,
            ..Default::default()
        };

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
        // `storageMode` used to choose between this app's own vault and a
        // shared external directory; that choice no longer exists (the vault
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
    fn clock_format_follows_the_system_by_default() {
        assert_eq!(Settings::default().clock_format, ClockFormat::System);
    }

    #[test]
    fn clock_format_round_trips_on_the_wire_as_12h_and_24h() {
        // The frontend switches on these exact strings, so the spelling is
        // part of the contract, not an implementation detail.
        for (value, wire) in [(ClockFormat::H12, "12h"), (ClockFormat::H24, "24h")] {
            let settings = Settings {
                clock_format: value,
                ..Settings::default()
            };
            let json = serde_json::to_value(&settings).unwrap();

            assert_eq!(json["clockFormat"], wire);
            assert_eq!(
                serde_json::from_value::<Settings>(json)
                    .unwrap()
                    .clock_format,
                value
            );
        }
    }

    #[test]
    fn an_unrecognised_clock_format_costs_one_field_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"threshold": 64, "clockFormat": "sundial"}"#,
        )
        .unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.clock_format, ClockFormat::default());
        assert_eq!(loaded.threshold, 64, "the rest of the file must survive");
    }

    #[test]
    fn the_alternate_spellings_of_a_clock_format_are_understood() {
        // What a human hand-editing the file, or another tool writing it,
        // would plausibly put there.
        for (raw, expected) in [
            ("12", ClockFormat::H12),
            ("12-hour", ClockFormat::H12),
            ("h12", ClockFormat::H12),
            ("ampm", ClockFormat::H12),
            ("24", ClockFormat::H24),
            ("24-hour", ClockFormat::H24),
            ("h23", ClockFormat::H24),
            ("military", ClockFormat::H24),
            ("auto", ClockFormat::System),
            ("locale", ClockFormat::System),
            (" 24H ", ClockFormat::H24),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                settings_path(dir.path()),
                format!(r#"{{"clockFormat": "{raw}"}}"#),
            )
            .unwrap();
            assert_eq!(load(dir.path()).clock_format, expected, "spelling {raw:?}");
        }
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

    #[test]
    fn settings_store_patch_changes_only_named_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());

        let result = store
            .update(
                0,
                SettingsPatch {
                    threshold: Some(77),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        assert_eq!(result.revision, 1);
        assert_eq!(result.settings.threshold, 77);
        assert_eq!(result.settings.theme, Theme::System);
        assert_eq!(result.settings.grace_seconds, 60);
    }

    #[test]
    fn settings_store_patch_applies_a_clock_format() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());

        let result = store
            .update(
                0,
                SettingsPatch {
                    clock_format: Some(ClockFormat::H24),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        assert_eq!(result.settings.clock_format, ClockFormat::H24);
        assert_eq!(load(dir.path()).clock_format, ClockFormat::H24);
    }

    #[test]
    fn settings_store_sanitises_patches_before_persisting_and_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());

        let result = store
            .update(
                0,
                SettingsPatch {
                    threshold: Some(255),
                    unhealthy_ticks: Some(0),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        assert_eq!(result.settings.threshold, 99);
        assert_eq!(result.settings.unhealthy_ticks, 1);
        assert_eq!(load(dir.path()).threshold, 99);
        assert_eq!(store.subscribe_policy().borrow().threshold, 99.0);
    }

    #[test]
    fn settings_patch_json_distinguishes_omission_null_and_value() {
        let omitted: SettingsPatch = serde_json::from_str("{}").unwrap();
        let cleared: SettingsPatch =
            serde_json::from_str(r#"{"autoSwitchPausedUntil":null,"claudeBinaryPath":null}"#)
                .unwrap();
        let until = fixed_now() + ChronoDuration::hours(1);
        let valued: SettingsPatch = serde_json::from_value(serde_json::json!({
            "autoSwitchPausedUntil": until,
            "claudeBinaryPath": "/x/claude",
        }))
        .unwrap();

        assert_eq!(omitted.auto_switch_paused_until, None);
        assert_eq!(cleared.auto_switch_paused_until, Some(None));
        assert_eq!(valued.auto_switch_paused_until, Some(Some(until)));

        assert_eq!(omitted.claude_binary_path, None);
        assert_eq!(cleared.claude_binary_path, Some(None));
        assert_eq!(
            valued.claude_binary_path,
            Some(Some("/x/claude".to_string()))
        );
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"futureSetting":true}"#).is_err());
    }

    #[test]
    fn settings_store_rejects_stale_revision_without_changing_state_or_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());
        store
            .update(
                0,
                SettingsPatch {
                    threshold: Some(78),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        let disk_before = std::fs::read(settings_path(dir.path())).unwrap();

        let error = store
            .update(
                0,
                SettingsPatch {
                    grace_seconds: Some(5),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            SettingsUpdateError::Conflict {
                expected_revision: 0,
                actual_revision: 1
            }
        ));
        assert_eq!(store.snapshot().revision, 1);
        assert_eq!(store.snapshot().settings.threshold, 78);
        assert_eq!(store.snapshot().settings.grace_seconds, 60);
        assert_eq!(
            std::fs::read(settings_path(dir.path())).unwrap(),
            disk_before
        );
    }

    #[test]
    fn settings_store_publishes_the_latest_complete_policy() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());
        let policy = store.subscribe_policy();

        store
            .update(
                0,
                SettingsPatch {
                    threshold: Some(81),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        store
            .update(
                1,
                SettingsPatch {
                    grace_seconds: Some(15),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        let current = policy.borrow().clone();
        assert_eq!(current.revision, 2);
        assert_eq!(current.threshold, 81.0);
        assert_eq!(current.grace.as_secs(), 15);
    }

    #[test]
    fn settings_store_failed_save_does_not_advance_or_publish() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("settings-parent-is-a-file");
        let store = SettingsStore::new(blocked.clone(), fixed_now());
        let policy = store.subscribe_policy();
        std::fs::write(&blocked, b"not a directory").unwrap();

        let error = store
            .update(
                0,
                SettingsPatch {
                    threshold: Some(75),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap_err();

        assert!(matches!(error, SettingsUpdateError::Persist(_)));
        assert_eq!(store.snapshot().revision, 0);
        assert_eq!(store.snapshot().settings.threshold, 90);
        assert_eq!(policy.borrow().revision, 0);
        assert_eq!(policy.borrow().threshold, 90.0);
    }

    #[test]
    fn settings_store_omitted_pause_preserves_it_and_explicit_null_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());
        let until = fixed_now() + ChronoDuration::hours(1);
        store
            .update(
                0,
                SettingsPatch {
                    auto_switch_paused_until: Some(Some(until)),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        let preserved = store
            .update(
                1,
                SettingsPatch {
                    threshold: Some(80),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        assert_eq!(preserved.settings.auto_switch_paused_until, Some(until));

        let cleared = store
            .update(
                2,
                SettingsPatch {
                    auto_switch_paused_until: Some(None),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        assert_eq!(cleared.settings.auto_switch_paused_until, None);
    }

    #[test]
    fn claude_binary_path_is_absent_by_default() {
        // No opinion on where `claude` lives until the user states one —
        // auto-discovery in `claude_cli` is the default, not this field.
        assert_eq!(Settings::default().claude_binary_path, None);
    }

    #[test]
    fn claude_binary_path_is_trimmed_and_blanked_to_none() {
        // A pasted path routinely carries leading/trailing whitespace from
        // copy-paste; whitespace-only input means "nothing configured".
        let trimmed = Settings {
            claude_binary_path: Some("  /x/claude ".to_string()),
            ..Settings::default()
        }
        .sanitised();
        assert_eq!(trimmed.claude_binary_path, Some("/x/claude".to_string()));

        let blanked = Settings {
            claude_binary_path: Some("   ".to_string()),
            ..Settings::default()
        }
        .sanitised();
        assert_eq!(blanked.claude_binary_path, None);
    }

    #[test]
    fn claude_binary_path_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            claude_binary_path: Some("/opt/claude/claude".to_string()),
            ..Settings::default()
        };

        save(dir.path(), &settings).unwrap();

        assert_eq!(
            load(dir.path()).claude_binary_path,
            Some("/opt/claude/claude".to_string())
        );
    }

    #[test]
    fn claude_binary_path_omitted_preserves_it_and_explicit_null_clears_it() {
        // Mirrors `settings_store_omitted_pause_preserves_it_and_explicit_null_clears_it`:
        // the same nested-Option contract has to hold for this field too.
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());
        store
            .update(
                0,
                SettingsPatch {
                    claude_binary_path: Some(Some("/x/claude".to_string())),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();

        let preserved = store
            .update(
                1,
                SettingsPatch {
                    threshold: Some(80),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        assert_eq!(
            preserved.settings.claude_binary_path,
            Some("/x/claude".to_string())
        );

        let cleared = store
            .update(
                2,
                SettingsPatch {
                    claude_binary_path: Some(None),
                    ..SettingsPatch::default()
                },
                fixed_now(),
            )
            .unwrap();
        assert_eq!(cleared.settings.claude_binary_path, None);
    }

    #[test]
    fn settings_store_snooze_and_resume_use_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().to_path_buf(), fixed_now());

        let snoozed = store
            .snooze(Duration::from_secs(3600), fixed_now())
            .unwrap();
        assert_eq!(
            snoozed.settings.auto_switch_paused_until,
            Some(fixed_now() + ChronoDuration::hours(1))
        );
        assert_eq!(snoozed.revision, 1);

        let resumed = store.resume(fixed_now()).unwrap();
        assert_eq!(resumed.settings.auto_switch_paused_until, None);
        assert_eq!(resumed.revision, 2);
    }
}
