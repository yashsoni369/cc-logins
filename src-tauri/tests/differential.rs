//! Differential tests against the `cswap` CLI as an independent oracle.
//!
//! Every other test in this crate is author-written: the same reasoning wrote
//! the code and the assertion, so a wrong assumption passes both. These tests
//! are different — they compare our model against a program we did not write,
//! reading the same machine state. That is the only check here capable of
//! catching a shared misunderstanding.
//!
//! It already earned its place: it is how we learned `usageAgeSeconds` is a
//! float (`422.4`) and not an integer, which made every account fail to parse.
//!
//! # Read-only by construction
//!
//! These tests invoke `cswap list --json` and `cswap status --json` ONLY.
//! They never call `switch`, `add`, `remove`, `enable`, `disable`, or `auto`,
//! and they never write to any credential store. Running the suite must be safe
//! against a live machine with real logged-in accounts. If you add a case here,
//! keep it to those two read subcommands.
//!
//! Skips cleanly when `cswap` is not installed, so CI without the CLI is green
//! rather than red-for-the-wrong-reason.

use std::process::Command;

use cc_logins_lib::model::{Account, UsageStatus};
use serde_json::Value;

/// The read-only subcommands this file is permitted to run.
const ALLOWED: [&str; 2] = ["list", "status"];

fn cswap_json(subcommand: &str) -> Option<Value> {
    assert!(
        ALLOWED.contains(&subcommand),
        "differential tests are read-only; '{subcommand}' is not an allowed subcommand"
    );
    let out = Command::new("cswap")
        .args([subcommand, "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn live_or_skip() -> Option<Value> {
    match cswap_json("list") {
        Some(v) => Some(v),
        None => {
            eprintln!("skipping: `cswap list --json` unavailable on this machine");
            None
        }
    }
}

/// The committed anonymized capture, so this file still asserts something
/// meaningful on a machine with no CLI and no accounts.
fn fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/snapshot.json"))
        .expect("committed fixture parses")
}

#[test]
fn every_account_the_cli_reports_deserialises_into_our_model() {
    let Some(v) = live_or_skip() else { return };
    let accounts = v["accounts"].as_array().expect("accounts array");
    assert!(
        !accounts.is_empty(),
        "expected at least one managed account"
    );

    for raw in accounts {
        let parsed: Result<Account, _> = serde_json::from_value(raw.clone());
        assert!(
            parsed.is_ok(),
            "account #{} failed to deserialise: {:?}\nraw: {}",
            raw["number"],
            parsed.err(),
            serde_json::to_string_pretty(raw).unwrap_or_default()
        );
    }
}

/// The strongest check here: anything the CLI emits that our model silently
/// drops is a field we will never show the user, and we would never notice.
#[test]
fn our_model_does_not_silently_drop_fields_the_cli_emits() {
    let live = live_or_skip();
    let v = live.unwrap_or_else(fixture);
    let accounts = v["accounts"].as_array().expect("accounts array");

    let mut dropped: Vec<String> = Vec::new();
    for raw in accounts {
        let parsed: Account = serde_json::from_value(raw.clone()).expect("account deserialises");
        let round_tripped = serde_json::to_value(&parsed).expect("account re-serialises");

        collect_missing("", raw, &round_tripped, &mut dropped);
    }

    dropped.sort();
    dropped.dedup();
    assert!(
        dropped.is_empty(),
        "our model drops fields the CLI reports: {dropped:?}\n\
         Either add them to model.rs or document why they are deliberately ignored."
    );
}

/// Walk the original against the round-trip, recording any key that vanished.
/// Absent-vs-null is treated as equivalent: `skip_serializing_if` legitimately
/// omits a key whose value was null.
fn collect_missing(path: &str, original: &Value, actual: &Value, out: &mut Vec<String>) {
    let Value::Object(orig) = original else {
        return;
    };
    for (k, v) in orig {
        let child = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        match actual.get(k) {
            None => {
                if !v.is_null() {
                    out.push(child);
                }
            }
            Some(a) => match (v, a) {
                (Value::Object(_), Value::Object(_)) => collect_missing(&child, v, a, out),
                (Value::Array(vs), Value::Array(as_)) => {
                    // Compare the first element; the arrays here are
                    // homogeneous (scoped windows).
                    if let (Some(v0), Some(a0)) = (vs.first(), as_.first()) {
                        collect_missing(&format!("{child}[]"), v0, a0, out);
                    }
                }
                _ => {}
            },
        }
    }
}

/// The tray renders one number: the worst window. If our idea of "worst"
/// diverges from the CLI's data, the tray lies.
#[test]
fn binding_utilisation_is_the_max_of_every_reported_window() {
    let live = live_or_skip();
    let v = live.unwrap_or_else(fixture);

    for raw in v["accounts"].as_array().expect("accounts array") {
        let parsed: Account = serde_json::from_value(raw.clone()).expect("deserialises");

        // Independently recompute from the raw JSON, without touching our
        // model's helpers — otherwise this just tests itself.
        let u = &raw["usage"];
        let mut expected: Option<f64> = None;
        for key in ["fiveHour", "sevenDay"] {
            if let Some(p) = u[key]["pct"].as_f64() {
                expected = Some(expected.map_or(p, |e: f64| e.max(p)));
            }
        }
        if let Some(scoped) = u["scoped"].as_array() {
            for s in scoped {
                if let Some(p) = s["pct"].as_f64() {
                    expected = Some(expected.map_or(p, |e: f64| e.max(p)));
                }
            }
        }

        assert_eq!(
            parsed.binding_utilisation(),
            expected,
            "binding window mismatch for account #{}",
            raw["number"]
        );
    }
}

/// Unknown usage must stay unknown. Collapsing it to 0.0 would make an
/// unmeasurable account look like the emptiest, and the auto-switcher would
/// preferentially jump to the one account it knows nothing about.
#[test]
fn accounts_without_usage_report_unknown_not_zero() {
    let mut a = Account {
        number: 9,
        email: "nobody@example.com".into(),
        usage_status: UsageStatus::Unknown,
        ..Default::default()
    };
    assert_eq!(a.binding_utilisation(), None);
    assert_eq!(a.headroom(), None);

    a.usage = Some(Default::default());
    assert_eq!(
        a.binding_utilisation(),
        None,
        "a present-but-empty usage object is still unknown"
    );
}

#[test]
fn the_active_account_is_unique_and_matches_status() {
    let Some(v) = live_or_skip() else { return };
    let accounts = v["accounts"].as_array().expect("accounts array");

    let actives: Vec<&Value> = accounts
        .iter()
        .filter(|a| a["active"].as_bool() == Some(true))
        .collect();
    assert!(
        actives.len() <= 1,
        "more than one account claims to be active: {actives:?}"
    );

    if let Some(active) = actives.first() {
        assert_eq!(
            v["activeAccountNumber"].as_u64(),
            active["number"].as_u64(),
            "activeAccountNumber disagrees with the account flagged active"
        );
    }

    // `status --json` is the CLI's own second opinion on the same question.
    if let Some(status) = cswap_json("status") {
        if let (Some(from_list), Some(from_status)) = (
            v["activeAccountNumber"].as_u64(),
            status["activeAccountNumber"]
                .as_u64()
                .or_else(|| status["number"].as_u64()),
        ) {
            assert_eq!(
                from_list, from_status,
                "`cswap list` and `cswap status` disagree about the active account"
            );
        }
    }
}

#[test]
fn schema_version_is_one_we_understand() {
    let live = live_or_skip();
    let v = live.unwrap_or_else(fixture);
    let version = v["schemaVersion"].as_u64().expect("schemaVersion present");
    assert_eq!(
        version, 1,
        "the CLI moved to schemaVersion {version}; re-read its JSON contract before trusting this reader"
    );
}
