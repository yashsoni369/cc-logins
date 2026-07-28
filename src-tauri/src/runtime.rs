use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::settings::{Settings, Strategy as SettingsStrategy};
use crate::switcher::Strategy;

/// The complete, immutable policy value observed by one poller revision.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePolicy {
    pub revision: u64,
    pub threshold: f64,
    pub cooldown: Duration,
    pub hysteresis_pct: f64,
    pub strategy: Strategy,
    pub unhealthy_ticks: u32,
    pub grace: Duration,
    pub auto_switch_enabled: bool,
    pub paused_until: Option<DateTime<Utc>>,
}

impl RuntimePolicy {
    pub fn from_settings(revision: u64, settings: &Settings, now: DateTime<Utc>) -> Self {
        let paused_until = settings
            .auto_switch_paused_until
            .filter(|until| *until > now);

        Self {
            revision,
            threshold: f64::from(settings.threshold),
            cooldown: Duration::from_secs(settings.cooldown_seconds),
            hysteresis_pct: f64::from(settings.hysteresis_pct),
            strategy: match settings.strategy {
                SettingsStrategy::MostHeadroom => Strategy::MostHeadroom,
                SettingsStrategy::NextAvailable => Strategy::NextAvailable,
                SettingsStrategy::ConsumeFirst => Strategy::ConsumeFirst,
            },
            unhealthy_ticks: u32::from(settings.unhealthy_ticks),
            grace: Duration::from_secs(settings.grace_seconds),
            auto_switch_enabled: settings.auto_switch_enabled,
            paused_until,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    use super::*;
    use crate::settings::{Settings, Strategy as SettingsStrategy};
    use crate::switcher::Strategy;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn an_expired_pause_is_absent_from_runtime_policy() {
        let now = fixed_now();
        let settings = Settings {
            auto_switch_paused_until: Some(now - ChronoDuration::seconds(1)),
            ..Settings::default()
        };

        let policy = RuntimePolicy::from_settings(3, &settings, now);

        assert_eq!(policy.paused_until, None);
    }

    #[test]
    fn an_active_pause_is_preserved_in_runtime_policy() {
        let now = fixed_now();
        let until = now + ChronoDuration::hours(1);
        let settings = Settings {
            auto_switch_paused_until: Some(until),
            ..Settings::default()
        };

        let policy = RuntimePolicy::from_settings(3, &settings, now);

        assert_eq!(policy.paused_until, Some(until));
    }

    #[test]
    fn runtime_policy_maps_every_daemon_setting() {
        let now = fixed_now();
        let settings = Settings {
            auto_switch_enabled: true,
            threshold: 83,
            cooldown_seconds: 91,
            hysteresis_pct: 7,
            unhealthy_ticks: 4,
            strategy: SettingsStrategy::ConsumeFirst,
            grace_seconds: 12,
            ..Settings::default()
        };

        let policy = RuntimePolicy::from_settings(11, &settings, now);

        assert_eq!(policy.threshold, 83.0);
        assert_eq!(policy.cooldown.as_secs(), 91);
        assert_eq!(policy.hysteresis_pct, 7.0);
        assert_eq!(policy.unhealthy_ticks, 4);
        assert_eq!(policy.strategy, Strategy::ConsumeFirst);
        assert_eq!(policy.grace.as_secs(), 12);
        assert!(policy.auto_switch_enabled);
    }

    #[test]
    fn runtime_policy_uses_the_supplied_revision() {
        let policy = RuntimePolicy::from_settings(27, &Settings::default(), fixed_now());

        assert_eq!(policy.revision, 27);
    }
}
