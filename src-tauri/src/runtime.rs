use std::{
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::Serialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DegradedReason {
    UsageUnknown,
    FetchFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum DaemonPhase {
    Disabled,
    Paused {
        until: DateTime<Utc>,
    },
    Monitoring,
    Cooldown {
        until: DateTime<Utc>,
    },
    Warning {
        from: u32,
        to: u32,
        deadline: DateTime<Utc>,
    },
    Switching {
        from: u32,
        to: u32,
    },
    Exhausted {
        earliest_reset: Option<DateTime<Utc>>,
    },
    Degraded {
        reason: DegradedReason,
    },
    RecoveryRequired {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub revision: u64,
    pub policy_revision: u64,
    pub phase: DaemonPhase,
    pub updated_at: DateTime<Utc>,
}

pub struct DaemonStatusStore {
    status: Mutex<DaemonStatus>,
}

impl DaemonStatusStore {
    pub fn new(policy: &RuntimePolicy, now: DateTime<Utc>) -> Self {
        let phase = if !policy.auto_switch_enabled {
            DaemonPhase::Disabled
        } else if let Some(until) = policy.paused_until {
            DaemonPhase::Paused { until }
        } else {
            DaemonPhase::Monitoring
        };
        Self {
            status: Mutex::new(DaemonStatus {
                revision: 0,
                policy_revision: policy.revision,
                phase,
                updated_at: now,
            }),
        }
    }

    pub fn snapshot(&self) -> DaemonStatus {
        self.lock_status().clone()
    }

    /// Store a changed status before returning it to the caller for emission.
    /// Identical phase and policy revisions are suppressed.
    pub fn transition(
        &self,
        policy_revision: u64,
        phase: DaemonPhase,
        now: DateTime<Utc>,
    ) -> Option<DaemonStatus> {
        let mut status = self.lock_status();
        if status.policy_revision == policy_revision && status.phase == phase {
            return None;
        }
        status.revision = status.revision.saturating_add(1);
        status.policy_revision = policy_revision;
        status.phase = phase;
        status.updated_at = now;
        Some(status.clone())
    }

    fn lock_status(&self) -> MutexGuard<'_, DaemonStatus> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    #[test]
    fn status_initial_phase_is_disabled_paused_or_monitoring_from_policy() {
        let now = fixed_now();
        let disabled = RuntimePolicy::from_settings(0, &Settings::default(), now);
        assert_eq!(
            DaemonStatusStore::new(&disabled, now).snapshot().phase,
            DaemonPhase::Disabled
        );

        let until = now + ChronoDuration::hours(1);
        let paused = RuntimePolicy::from_settings(
            4,
            &Settings {
                auto_switch_enabled: true,
                auto_switch_paused_until: Some(until),
                ..Settings::default()
            },
            now,
        );
        let paused_status = DaemonStatusStore::new(&paused, now).snapshot();
        assert_eq!(paused_status.phase, DaemonPhase::Paused { until });
        assert_eq!(paused_status.policy_revision, 4);

        let monitoring = RuntimePolicy::from_settings(
            8,
            &Settings {
                auto_switch_enabled: true,
                ..Settings::default()
            },
            now,
        );
        assert_eq!(
            DaemonStatusStore::new(&monitoring, now).snapshot().phase,
            DaemonPhase::Monitoring
        );
    }

    #[test]
    fn status_transitions_increment_revision_and_store_before_returning() {
        let now = fixed_now();
        let policy = RuntimePolicy::from_settings(
            3,
            &Settings {
                auto_switch_enabled: true,
                ..Settings::default()
            },
            now,
        );
        let store = DaemonStatusStore::new(&policy, now);
        let until = now + ChronoDuration::minutes(5);

        let changed = store
            .transition(3, DaemonPhase::Cooldown { until }, now)
            .unwrap();

        assert_eq!(changed.revision, 1);
        assert_eq!(changed.phase, DaemonPhase::Cooldown { until });
        assert_eq!(store.snapshot(), changed);
    }

    #[test]
    fn status_identical_transition_is_suppressed() {
        let now = fixed_now();
        let policy = RuntimePolicy::from_settings(
            2,
            &Settings {
                auto_switch_enabled: true,
                ..Settings::default()
            },
            now,
        );
        let store = DaemonStatusStore::new(&policy, now);

        let changed = store.transition(
            2,
            DaemonPhase::Monitoring,
            now + ChronoDuration::seconds(30),
        );

        assert_eq!(changed, None);
        assert_eq!(store.snapshot().revision, 0);
        assert_eq!(store.snapshot().updated_at, now);
    }

    #[test]
    fn status_policy_revision_change_publishes_even_when_phase_is_unchanged() {
        let now = fixed_now();
        let policy = RuntimePolicy::from_settings(
            2,
            &Settings {
                auto_switch_enabled: true,
                ..Settings::default()
            },
            now,
        );
        let store = DaemonStatusStore::new(&policy, now);

        let changed = store.transition(3, DaemonPhase::Monitoring, now).unwrap();

        assert_eq!(changed.revision, 1);
        assert_eq!(changed.policy_revision, 3);
    }

    #[test]
    fn status_serializes_as_exact_camel_case_tagged_contract() {
        let now = fixed_now();
        let status = DaemonStatus {
            revision: 9,
            policy_revision: 4,
            phase: DaemonPhase::Warning {
                from: 1,
                to: 7,
                deadline: now + ChronoDuration::seconds(60),
            },
            updated_at: now,
        };

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({
                "revision": 9,
                "policyRevision": 4,
                "phase": {
                    "kind": "warning",
                    "from": 1,
                    "to": 7,
                    "deadline": "2026-07-28T12:01:00Z"
                },
                "updatedAt": "2026-07-28T12:00:00Z"
            })
        );

        assert_eq!(
            serde_json::to_value(DaemonPhase::Degraded {
                reason: DegradedReason::UsageUnknown
            })
            .unwrap(),
            serde_json::json!({ "kind": "degraded", "reason": "usageUnknown" })
        );
        assert_eq!(
            serde_json::to_value(DaemonPhase::Exhausted {
                earliest_reset: None
            })
            .unwrap(),
            serde_json::json!({ "kind": "exhausted", "earliestReset": null })
        );
    }
}
