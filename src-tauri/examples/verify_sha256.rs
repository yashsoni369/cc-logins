//! Throwaway check that the sha2 0.11 upgrade did not change hex output.
//!
//! Deliberately an example, not a #[test]: the test suite touches real user
//! paths. Run with `cargo run --example verify_sha256`.

use cc_logins_lib::{hex, model::Account, oauth_refresh, switch_journal};

fn check(label: &str, got: &str, want: &str) -> bool {
    let ok = got == want;
    println!(
        "{} {label}\n   got  {got}\n   want {want}",
        if ok { "PASS" } else { "FAIL" }
    );
    ok
}

fn main() {
    let mut all_ok = true;

    // NIST/RFC-standard SHA-256 vectors, independent of this codebase.
    all_ok &= check(
        "hex::lower(SHA-256(\"abc\"))",
        &hex::lower(&<sha2::Sha256 as sha2::Digest>::digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
    all_ok &= check(
        "hex::lower(SHA-256(\"\"))",
        &hex::lower(&<sha2::Sha256 as sha2::Digest>::digest(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );

    // The identity key the TypeScript side recomputes via crypto.subtle.
    // SHA-256("user@example.com") per an independent implementation.
    let account = Account {
        email: "  User@Example.COM  ".to_string(),
        ..Default::default()
    };
    all_ok &= check(
        "Account::stable_key (trim + lowercase + sha256)",
        &account.stable_key(),
        "email:b4c9a289323b21a01c3e940f150eb9b8c542587f1abfd8f0e1cc1ffc5e475514",
    );

    // Org UUID path must still bypass hashing entirely.
    let org = Account {
        email: "user@example.com".to_string(),
        organization_uuid: Some("  abc-123  ".to_string()),
        ..Default::default()
    };
    all_ok &= check(
        "Account::stable_key (org uuid)",
        &org.stable_key(),
        "org:abc-123",
    );

    // Journal artifact hashes: persisted in switch journals on disk.
    all_ok &= check(
        "switch_journal::sha256(b\"abc\")",
        &switch_journal::sha256(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );

    // Credential generation markers: compared against values already stored.
    all_ok &= check(
        "oauth_refresh::credential_generation(\"abc\")",
        &oauth_refresh::credential_generation("abc"),
        "sha256-full:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );

    println!(
        "\n{}",
        if all_ok {
            "ALL PASS - hex output is byte-identical under sha2 0.11"
        } else {
            "FAILURE - hex output changed"
        }
    );
    assert!(all_ok, "sha256 hex output changed under sha2 0.11");
}
