//! The shared account/usage model.
//!
//! This is the single definition of the shapes that cross the Tauri IPC
//! boundary. It mirrors `src/types.ts` field for field — serde emits camelCase
//! so the TypeScript side needs no translation layer.
//!
//! The field names also match the `cswap --json` contract (schemaVersion 1),
//! which lets us diff our reader against the CLI on a real machine and assert
//! they describe the same state. That differential test is the main defence
//! against a subtly wrong port.
//!
//! Forward-compatibility rule inherited from that contract: unknown fields are
//! ignored, never rejected, so a newer producer cannot break an older consumer.

use serde::{Deserialize, Serialize};

/// One rate-limit window.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// Utilisation 0..=100. Not headroom.
    pub pct: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resets_at: Option<String>,
    /// Recomputed at render time; a cached countdown drifts as it ages.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub countdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub clock: Option<String>,

    // Pace projection. Present on the seven-day and scoped windows.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expected_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ahead_of_pace: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub projected_exhaustion_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub will_last_to_reset: Option<bool>,

    /// Set only on per-model scoped windows, e.g. "Fable".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub five_hour: Option<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub seven_day: Option<UsageWindow>,
    /// Per-model weekly limits. Absent on older API responses.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scoped: Option<Vec<UsageWindow>>,
}

/// Why an account cannot currently be used or measured.
///
/// The full variant set is whatever the upstream CLI emits, which we do not
/// control. Deserialisation is therefore **open**: an unrecognised status
/// degrades to [`UsageStatus::Unknown`] rather than failing the parse.
///
/// This is not defensive padding. A live differential run caught the CLI
/// emitting `"unavailable"` — a status absent from the captured fixture,
/// because it only appears during a transient usage-fetch failure. With a
/// closed enum, one unrecognised string made the *entire account* fail to
/// deserialise, so a momentary network blip would have blanked the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageStatus {
    Ok,
    /// Live OAuth bytes were positively attributed to a different account.
    ForeignCredential,
    /// Current credential generation was server-rejected; re-login required.
    ReloginRequired,
    /// Could not be read this cycle; last-known values shown.
    Stale,
    /// Held out of auto-rotation by the user.
    Disabled,
    /// Usage could not be retrieved for this account right now. Transient.
    Unavailable,
    /// The CLI reported an error state for this account.
    Error,
    /// Never successfully measured, or a status this build does not know.
    #[default]
    Unknown,
}

impl<'de> Deserialize<'de> for UsageStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "ok" => Self::Ok,
            "foreigncredential" | "foreign-credential" => Self::ForeignCredential,
            "reloginrequired" | "expired" => Self::ReloginRequired,
            "stale" => Self::Stale,
            "disabled" => Self::Disabled,
            "unavailable" => Self::Unavailable,
            "error" => Self::Error,
            // Forward-compatible: a status added upstream must never cost us
            // the whole account.
            _ => Self::Unknown,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub number: u32,
    pub email: String,
    /// Stable profile identity used for credential provenance checks. It is
    /// backend-only and must not be exposed in the snapshot payload.
    #[serde(skip)]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub organization_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub organization_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub is_organization: Option<bool>,
    pub active: bool,
    pub usage_status: UsageStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_fetched_at: Option<String>,
    /// Fractional: the real CLI emits values like `422.4`, so this cannot be an
    /// integer type. Caught by the fixture test against a live capture.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub usage_age_seconds: Option<f64>,
}

impl Account {
    /// Stable identity for history rows.
    ///
    /// Slot number and email both change over an account's life — slots are
    /// user-reorderable and emails get renamed — so neither can key a time
    /// series. The org UUID is stable when present; otherwise fall back to a
    /// hash of the lowercased email and accept that a rename starts a new
    /// series.
    ///
    /// The email is hashed rather than stored raw because this key lands in a
    /// local SQLite file that is local but not necessarily private — it travels
    /// in bug reports and screenshots. `crate::history` calls this method
    /// directly for every history row's identity, so any change here is a
    /// schema change that would orphan existing history.
    pub fn stable_key(&self) -> String {
        use sha2::{Digest, Sha256};
        match self.organization_uuid.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => format!("org:{u}"),
            _ => {
                // Trim before hashing: a stray space around an address is the
                // same account, and without this a padded email forks the
                // history series.
                let digest = Sha256::digest(self.email.trim().to_ascii_lowercase().as_bytes());
                format!("email:{digest:x}")
            }
        }
    }

    /// Preferred display name. People screenshot this app, so an alias always
    /// wins over an email.
    pub fn display_name(&self) -> String {
        match self.alias.as_deref().map(str::trim) {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => mask_email(&self.email),
        }
    }

    /// Every window that gates this account, as `(label, pct)`.
    pub fn binding_windows(&self) -> Vec<(String, f64)> {
        let Some(u) = &self.usage else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(w) = &u.five_hour {
            out.push(("5h".to_string(), w.pct));
        }
        if let Some(w) = &u.seven_day {
            out.push(("7d".to_string(), w.pct));
        }
        for s in u.scoped.iter().flatten() {
            out.push((s.name.clone().unwrap_or_else(|| "model".into()), s.pct));
        }
        out
    }

    /// Utilisation of the binding window — the highest one. This is the number
    /// the tray renders.
    ///
    /// `None` means unknown, which callers must treat as "never auto-skip".
    /// Returning 0.0 here would silently make an unmeasurable account look like
    /// the best switch target.
    pub fn binding_utilisation(&self) -> Option<f64> {
        self.binding_windows()
            .into_iter()
            .map(|(_, p)| p)
            .fold(None, |acc: Option<f64>, p| {
                Some(acc.map_or(p, |a| a.max(p)))
            })
    }

    /// Remaining percentage before this account hits a limit.
    pub fn headroom(&self) -> Option<f64> {
        self.binding_utilisation().map(|u| 100.0 - u)
    }

    /// Eligible as an auto-switch target.
    pub fn is_switchable(&self) -> bool {
        !self.active
            && !matches!(
                self.usage_status,
                UsageStatus::Disabled | UsageStatus::ReloginRequired
            )
    }

    /// Eligible for unattended activation. Automatic selection is
    /// deliberately stricter than manual switching: the usage measurement
    /// must be fresh, successful, and prove positive headroom.
    pub fn is_automatic_target(&self) -> bool {
        !self.active
            && self.usage_status == UsageStatus::Ok
            && matches!(self.headroom(), Some(value) if value > 0.0)
    }
}

/// Mask an email for display. Screenshots of this app are common.
pub fn mask_email(email: &str) -> String {
    match email.find('@') {
        Some(at) if at > 0 => {
            let first = email.chars().next().unwrap_or('?');
            format!("{first}•••{}", &email[at..])
        }
        _ => email.to_string(),
    }
}

/// A distinct credential store: native OS, a WSL distro, or a profile dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvKind {
    Native,
    Wsl,
    Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvStatus {
    /// Readable right now.
    Live,
    /// A stopped WSL distro. Reading it would boot the VM, so it is never
    /// polled — values are last-known and waking is an explicit user action.
    Asleep,
    /// No Claude Code install found here.
    Ignored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: String,
    pub label: String,
    pub path: String,
    pub kind: EnvKind,
    pub status: EnvStatus,
    pub accounts: Vec<Account>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_seen_seconds: Option<u64>,
    /// Whether Claude Code credentials were found in this realm, independent
    /// of `accounts` (which may not be populated for every environment kind).
    /// `None` means "not determined" — e.g. this realm was never probed, or
    /// probing it would have required touching a stopped WSL distro's
    /// filesystem, which is never done from a polling path. Distinct from
    /// `Some(false)`, which means the probe ran and found nothing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub has_credentials: Option<bool>,
}

/// Everything the UI needs in one payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_account_number: Option<u32>,
    pub environments: Vec<Environment>,
}

impl Snapshot {
    pub fn new(environments: Vec<Environment>) -> Self {
        let active_account_number = environments
            .iter()
            .find(|e| e.kind == EnvKind::Native)
            .and_then(|e| e.accounts.iter().find(|a| a.active))
            .map(|a| a.number);
        Self {
            schema_version: 1,
            active_account_number,
            environments,
        }
    }

    /// The account whose credentials are live in the native environment.
    pub fn active_account(&self) -> Option<&Account> {
        self.environments
            .iter()
            .flat_map(|e| e.accounts.iter())
            .find(|a| a.active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(pct: f64) -> UsageWindow {
        UsageWindow {
            pct,
            ..Default::default()
        }
    }

    fn acct(pcts: (f64, f64), scoped: &[f64]) -> Account {
        Account {
            number: 1,
            email: "alpha@example.com".into(),
            usage: Some(Usage {
                five_hour: Some(win(pcts.0)),
                seven_day: Some(win(pcts.1)),
                scoped: Some(
                    scoped
                        .iter()
                        .map(|p| UsageWindow {
                            pct: *p,
                            name: Some("Fable".into()),
                            ..Default::default()
                        })
                        .collect(),
                ),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn binding_window_is_the_worst_one() {
        // The seven-day window binds even though the five-hour is nearly idle.
        let a = acct((16.0, 81.0), &[22.0]);
        assert_eq!(a.binding_utilisation(), Some(81.0));
        assert_eq!(a.headroom(), Some(19.0));
    }

    #[test]
    fn a_maxed_model_binds_even_with_general_headroom() {
        let a = acct((10.0, 20.0), &[100.0]);
        assert_eq!(a.binding_utilisation(), Some(100.0));
    }

    #[test]
    fn unknown_usage_is_none_not_zero() {
        // Zero would make an unmeasurable account look like the best target.
        let a = Account {
            number: 1,
            email: "a@b.com".into(),
            ..Default::default()
        };
        assert_eq!(a.binding_utilisation(), None);
        assert_eq!(a.headroom(), None);
    }

    #[test]
    fn stable_key_prefers_org_uuid_over_email() {
        let mut a = acct((0.0, 0.0), &[]);
        a.organization_uuid = Some("abc-123".into());
        assert_eq!(a.stable_key(), "org:abc-123");

        a.organization_uuid = None;
        let lower = a.stable_key();
        assert!(lower.starts_with("email:"));
        // The raw address must never appear: this key is written to a local
        // SQLite file that travels in bug reports and screenshots.
        assert!(!lower.contains("alpha@example.com"));

        // Case must not fork the series.
        a.email = "Alpha@Example.com".into();
        assert_eq!(a.stable_key(), lower);
    }

    #[test]
    fn masking_keeps_the_domain() {
        assert_eq!(mask_email("user@example.com"), "u•••@example.com");
        assert_eq!(mask_email("setup-token-4"), "setup-token-4");
        assert_eq!(mask_email("@weird.com"), "@weird.com");
    }

    #[test]
    fn display_name_prefers_alias() {
        let mut a = acct((0.0, 0.0), &[]);
        a.alias = Some("tekyz".into());
        assert_eq!(a.display_name(), "tekyz");
        a.alias = Some("   ".into());
        assert_eq!(a.display_name(), "a•••@example.com");
    }

    #[test]
    fn manual_and_automatic_target_eligibility_are_distinct() {
        let mut a = acct((0.0, 0.0), &[]);
        a.usage_status = UsageStatus::Ok;
        assert!(a.is_switchable());
        assert!(a.is_automatic_target());
        a.usage_status = UsageStatus::Stale;
        assert!(a.is_switchable());
        assert!(!a.is_automatic_target());
        a.usage_status = UsageStatus::Unknown;
        assert!(a.is_switchable());
        assert!(!a.is_automatic_target());
        a.usage_status = UsageStatus::ForeignCredential;
        assert!(a.is_switchable());
        assert!(!a.is_automatic_target());
        a.usage_status = UsageStatus::Disabled;
        assert!(!a.is_switchable());
        assert!(!a.is_automatic_target());
        a.usage_status = UsageStatus::ReloginRequired;
        assert!(!a.is_switchable());
        assert!(!a.is_automatic_target());
        // The active account is never its own switch target.
        a.usage_status = UsageStatus::Ok;
        a.active = true;
        assert!(!a.is_switchable());
        assert!(!a.is_automatic_target());
    }

    #[test]
    fn relogin_required_serializes_explicitly_and_accepts_legacy_expired() {
        assert_eq!(
            serde_json::to_string(&UsageStatus::ReloginRequired).unwrap(),
            r#""reloginrequired""#
        );
        assert_eq!(
            serde_json::from_str::<UsageStatus>(r#""reloginrequired""#).unwrap(),
            UsageStatus::ReloginRequired
        );
        assert_eq!(
            serde_json::from_str::<UsageStatus>(r#""expired""#).unwrap(),
            UsageStatus::ReloginRequired
        );
    }

    #[test]
    fn foreign_credential_status_round_trips_explicitly() {
        assert_eq!(
            serde_json::to_string(&UsageStatus::ForeignCredential).unwrap(),
            r#""foreigncredential""#
        );
        assert_eq!(
            serde_json::from_str::<UsageStatus>(r#""foreign-credential""#).unwrap(),
            UsageStatus::ForeignCredential
        );
    }

    #[test]
    fn deserialises_the_real_cswap_fixture() {
        // Ground truth: an anonymized capture of `cswap list --json` from a
        // machine with three real accounts.
        let raw = include_str!("../../fixtures/snapshot.json");
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture parses");
        let accounts = v["accounts"].as_array().expect("accounts array");
        assert_eq!(accounts.len(), 3);

        for a in accounts {
            let parsed: Account = serde_json::from_value(a.clone()).expect("account deserialises");
            assert!(!parsed.email.is_empty());
            // Every account in the fixture carries a scoped per-model window.
            let scoped = parsed.usage.as_ref().and_then(|u| u.scoped.as_ref());
            assert!(
                scoped.is_some_and(|s| !s.is_empty()),
                "scoped windows present"
            );
        }

        // The active account in the capture is slot 3 at 7d 81%.
        let active: Account = serde_json::from_value(accounts[2].clone()).unwrap();
        assert!(active.active);
        assert_eq!(active.binding_utilisation(), Some(81.0));
    }
}
