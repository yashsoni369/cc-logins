//! Credential storage layer for cc-logins.
//!
//! Ported from claude-swap (MIT) — <https://github.com/realiti4/claude-swap>,
//! `claude_swap/credentials.py`. Owns *where* Claude Code's credentials live
//! and *how* they are read/written: the macOS Keychain-vs-file routing,
//! per-process capability detection and sticky fallback, and the
//! `.enc`-wins backup reconciliation between the Keychain and the file
//! backup vault.
//!
//! [`CredentialStore`] is a leaf collaborator, mirroring the Python class of
//! the same name: it never assumes ownership of account/slot orchestration
//! (that belongs to a future `switcher` port) and reads its live
//! configuration (`platform`, `credentials_dir`) from a small [`StoreHost`]
//! view, exactly as the Python version reads `self._host.platform` /
//! `self._host.credentials_dir` at call time rather than snapshotting them at
//! construction.
//!
//! # Hard rule inherited from upstream
//!
//! **Never hold a credential or config lock (see [`crate::locking`]) across a
//! network call.** Every method here is pure local file/Keychain I/O — no
//! network access — so it is always safe to call while holding a
//! `crate::locking::FileLock`. A future caller that layers OAuth refresh on
//! top of this module must acquire the lock, do the local read/write, release
//! it, and only *then* make any HTTP call.
//!
//! # What is faithfully ported vs. adapted
//!
//! The path layout, retry/backoff constants, and every branch of the
//! Keychain-vs-file decision tree are ported 1:1 from the Python source.
//! What differs is necessarily the shape of the API: Python's exceptions
//! become [`CredentialError`] variants, and the `_StoreHost` `Protocol`
//! becomes the [`StoreHost`] trait. See the crate-level report for the
//! macOS Keychain caveats (this file talks to Security.framework via the
//! `security-framework` crate rather than shelling out to
//! `/usr/bin/security` as the Python version does).
//!
//! One on-disk format is a deliberate, intentional departure from upstream:
//! the `.enc` vault files (per-account backups, `.enc.prev` generations, and
//! unclaimed-credential stash entries) are no longer raw base64 — base64 is
//! obfuscation, not encryption. They are now a small versioned JSON envelope
//! applying real platform-native protection (Windows DPAPI; honest
//! `"plain"` 0600 files elsewhere a native primitive isn't available); see
//! the envelope doc comment near [`CredentialEnvelope`] and
//! [`protection_scheme`]. This only ever existed as raw base64 because the
//! vault used to need to be byte-compatible with the upstream Python tool's
//! own format; that constraint is gone now that this vault is this app's
//! own, so [`unwrap_credential`] still reads the old raw-base64 shape
//! transparently, but every write upgrades to the envelope.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Constants (mirroring the module-level constants in credentials.py).
// ---------------------------------------------------------------------------

/// Service name for per-account backup credentials in the macOS Keychain.
/// Deliberately distinct from any legacy `keyring`-based service name so old
/// and new items can coexist during a migration.
pub const SECURITY_SERVICE: &str = "claude-swap";

/// This app's OWN Keychain service, distinct from the CLI's above.
///
/// Sharing `claude-swap` made `import_from_cswap` overwrite the CLI's backups
/// whenever slot number and email matched — the normal case — contradicting its
/// documented "read-only on the source" guarantee on macOS only.
pub const GUI_SECURITY_SERVICE: &str = "cc-logins";

/// Service name of Claude Code's *active* OAuth credential in the macOS
/// Keychain (read by Claude Code itself; we read/write it when switching
/// accounts).
pub const CLAUDE_CODE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Service name of Claude Code's *active* managed API key
/// (`sk-ant-api…`, activated via `/login`) in the macOS Keychain. Distinct
/// from the OAuth service above (no `-credentials` suffix) — Claude Code
/// resolves it on a separate auth axis. On non-macOS the managed key instead
/// lives in `~/.claude.json` as `primaryApiKey`.
pub const CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE: &str = "Claude Code";

/// Bounded retry for the active OAuth-credential Keychain read: a
/// locked/contended login Keychain can fail a single call transiently, and a
/// second attempt shortly after usually succeeds.
const ACTIVE_READ_ATTEMPTS: u32 = 2;
const ACTIVE_READ_RETRY_DELAY: Duration = Duration::from_millis(300);

/// After a Keychain failure the store drops to file mode so one process
/// invocation can't split-brain between backends; a long-running process
/// re-probes after this cooldown so a transient failure self-heals instead of
/// disabling the Keychain for the whole process lifetime.
const KEYCHAIN_RECHECK_COOLDOWN: Duration = Duration::from_secs(60);

/// The credential object's siblings of `claudeAiOauth` that are
/// machine-shared OAuth integrations rotating independently of any account
/// slot, so on activation the live copy is authoritative over a slot's
/// snapshot. Everything else stays slot-owned.
pub const SHARED_CREDENTIAL_KEYS: &[&str] = &[
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

/// Account-scoped siblings of `claudeAiOauth` this store knows about, named
/// so the unrecognized-key probe in [`shared_credential_fields`] doesn't flag
/// them.
pub const ACCOUNT_CREDENTIAL_KEYS: &[&str] = &["claudeAiOauth", "trustedDeviceToken"];

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// Errors from [`CredentialStore`] operations.
///
/// Mirrors the raised-exception surface of `credentials.py`
/// (`CredentialError` / `CredentialWriteError`) but as a closed enum instead
/// of a class hierarchy of stringly-messaged exceptions.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A credential *write* failed outright (Python's `CredentialWriteError`).
    #[error("failed to write credentials: {0}")]
    Write(String),

    #[error("could not snapshot exact active credential state: {0}")]
    Snapshot(String),

    #[error("could not restore exact active credential state: {0}")]
    Restore(String),

    #[error("active credential state does not match the recovery snapshot")]
    VerificationFailed,

    /// A destination that must end up empty could not be verified empty
    /// (Python's `delete_account_credentials_strict`, which fails closed
    /// rather than risk resurfacing another account's material).
    #[error("could not clear stored credentials for slot {account_num} ({email}): {reason}")]
    ClearFailed {
        account_num: String,
        email: String,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Platform.
// ---------------------------------------------------------------------------

/// Use the canonical `Platform` enum from `crate::paths`. See that module's
/// docstring for platform-detection logic and the XDG layout rationale.
pub use crate::paths::Platform;

/// The live configuration a [`CredentialStore`] reads from its owner, e.g. a
/// future `switcher` port. Mirrors the `_StoreHost` `Protocol` in
/// credentials.py: intentionally data-only (no orchestration methods), and
/// read at call time so a caller can change `platform`/`credentials_dir`
/// between calls (the Python docstring calls this out explicitly — tests
/// override `switcher.platform` post-construction and the store must observe
/// it).
///
/// Logging is *not* part of this trait (unlike the Python `_logger` field):
/// this port uses the crate-wide `log` facade directly, since Rust's `log`
/// crate is already a global sink rather than an object needing to be threaded
/// through.
pub trait StoreHost {
    fn platform(&self) -> Platform;
    fn credentials_dir(&self) -> PathBuf;

    /// Keychain service name for this host's per-account backups.
    ///
    /// Keychain items are keyed by service + account only — `credentials_dir`
    /// is ignored on that backend — so two hosts sharing a service name share
    /// one namespace no matter how separate their directories are. Defaults to
    /// the CLI's, which is right for [`CswapStoreHost`]; this app overrides it.
    fn keychain_service(&self) -> &str {
        SECURITY_SERVICE
    }
}

/// Which backend the most recent active-credential write landed on. Mirrors
/// the `"keychain" | "file"` strings Python stashes in
/// `_last_active_credentials_backend` for the post-switch follow-up message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    File,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Keychain => "keychain",
            Backend::File => "file",
        }
    }
}

/// Outcome of reading Claude Code's active credential.
///
/// `value` is the credential string (OAuth JSON or a raw managed key),
/// `Some(String::new())` when none exists in any backend, or `None` on a
/// plaintext-file read error — this three-way split mirrors Python's
/// `value: str | None` where `""` and `None` are meaningfully different.
/// `keychain_unavailable` is true only when the macOS OAuth Keychain read
/// failed (locked / denied / timeout) and nothing else covered it, letting
/// callers distinguish a transiently unreadable Keychain from a genuinely
/// empty slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCredentials {
    pub value: Option<String>,
    pub keychain_unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "presence", content = "value")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the pending switch transaction slice")
)]
pub(crate) enum EntryState<T> {
    Absent,
    Present(T),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the pending switch transaction slice")
)]
pub(crate) struct ActiveCredentialState {
    pub credentials_file: EntryState<Vec<u8>>,
    pub oauth_keychain: EntryState<String>,
    pub managed_keychain: EntryState<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers — no I/O, unit-testable in isolation.
// ---------------------------------------------------------------------------

/// Whether a stored active credential is a raw managed API key vs OAuth JSON.
///
/// Strict on purpose: a managed key is a bare `sk-ant-api…` string, while
/// every OAuth/setup-token credential is a JSON object
/// (`{"claudeAiOauth": …}`). Requiring the `sk-ant-api` prefix (and that it
/// isn't JSON) keeps a raw/garbled `sk-ant-oat…` setup token from ever being
/// misclassified as an API key.
pub fn looks_like_api_key(credentials: Option<&str>) -> bool {
    let Some(raw) = credentials else {
        return false;
    };
    if raw.is_empty() {
        return false;
    }
    let text = raw.trim();
    text.starts_with("sk-ant-api") && !text.starts_with('{')
}

/// Parse a JSON credential object, excluding managed API keys. `None` for
/// missing/empty input, a managed key, malformed JSON, or JSON that isn't an
/// object.
fn credential_object(credentials: Option<&str>) -> Option<Map<String, Value>> {
    let raw = credentials?;
    if raw.is_empty() || looks_like_api_key(Some(raw)) {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Return the machine-shared fields of a Claude OAuth credential object.
///
/// Only the [`SHARED_CREDENTIAL_KEYS`] allowlist is machine-shared; other
/// siblings of `claudeAiOauth` are account-scoped or unknown and stay
/// slot-owned. `None` means the input is not a JSON credential object
/// (missing, malformed, or a managed API key). A map — including an empty
/// one — is authoritative for every allowlisted key: a key absent here is
/// absent from the machine's current shared state.
pub fn shared_credential_fields(credentials: Option<&str>) -> Option<Map<String, Value>> {
    let data = credential_object(credentials)?;
    if data.contains_key("claudeAiOauth") {
        let mut unrecognized: Vec<&str> = data
            .keys()
            .map(String::as_str)
            .filter(|k| !SHARED_CREDENTIAL_KEYS.contains(k) && !ACCOUNT_CREDENTIAL_KEYS.contains(k))
            .collect();
        if !unrecognized.is_empty() {
            unrecognized.sort_unstable();
            log::debug!(
                "Live credential has sibling keys cswap does not recognize \
                 (a newer Claude Code?), treating them as slot-owned: {unrecognized:?}"
            );
        }
    }
    let mut result = Map::new();
    for key in SHARED_CREDENTIAL_KEYS {
        if let Some(v) = data.get(*key) {
            result.insert((*key).to_string(), v.clone());
        }
    }
    Some(result)
}

/// Compose a target Claude login with the machine's shared fields.
///
/// The allowlisted keys are wholly live-owned, presence and absence alike:
/// the target's copies are discarded and `shared_fields` supplies the
/// current generation, so a shared key the machine no longer holds is not
/// resurrected from the slot's snapshot. All other target fields pass
/// through untouched. Returns `target_credentials` unchanged when it is not
/// a JSON credential object carrying a Claude login (managed API keys and
/// opaque legacy shapes stay activatable verbatim).
pub fn merge_shared_credential_fields(
    target_credentials: &str,
    shared_fields: &Map<String, Value>,
) -> String {
    let target = match credential_object(Some(target_credentials)) {
        Some(t) if t.contains_key("claudeAiOauth") => t,
        _ => return target_credentials.to_string(),
    };

    let mut composed: Map<String, Value> = target
        .into_iter()
        .filter(|(k, _)| !SHARED_CREDENTIAL_KEYS.contains(&k.as_str()))
        .collect();
    for (k, v) in shared_fields {
        composed.insert(k.clone(), v.clone());
    }
    // Re-serializing a Map<String, Value> built entirely from previously
    // parsed JSON cannot fail; the fallback is defensive only.
    serde_json::to_string(&Value::Object(composed))
        .unwrap_or_else(|_| target_credentials.to_string())
}

/// The value Claude Code stores in `customApiKeyResponses.approved`.
///
/// Mirrors Claude Code's `normalizeApiKeyForConfig`
/// (`apiKey.slice(-20)`): the last 20 *characters* (not bytes — matches
/// Python's codepoint-indexed `str[-20:]`). Storing anything else makes
/// Claude Code's "is this key approved?" check miss and re-prompt the user
/// to approve the key.
pub fn approved_form(api_key: &str) -> String {
    let chars: Vec<char> = api_key.trim().chars().collect();
    let start = chars.len().saturating_sub(20);
    chars[start..].iter().collect()
}

// ---------------------------------------------------------------------------
// Atomic file I/O helpers.
// ---------------------------------------------------------------------------

/// Atomically write `contents` to `target`: write to a sibling temp file in
/// the same directory (so the final rename is same-filesystem and therefore
/// atomic), then rename over the destination. 0600 on non-Windows, mirroring
/// every `sys.platform != "win32": os.chmod(path, 0o600)` guard in the Python
/// source — skipped on Windows because ACL-based permissions don't map onto
/// POSIX mode bits there.
///
/// `tempfile::NamedTempFile` is intentionally not used here: `tempfile` is a
/// dev-dependency only (used by this crate's tests), not a runtime one, so
/// production code hand-rolls the same "unique name in the target directory,
/// create_new, write, rename, cleanup on failure" shape Python's
/// `tempfile.mkstemp(dir=..., suffix=".tmp")` provides.
fn atomic_write(target: &Path, contents: &[u8]) -> io::Result<()> {
    crate::durable_fs::stage_sibling(target, contents, Some(0o600))?
        .commit()
        .map_err(Into::into)
}

/// Best-effort process-local randomness for the unclaimed-credential entry
/// id's uniqueness nonce (Python: `secrets.token_hex(3)`).
///
/// No RNG crate (`rand`, `getrandom`) is in `Cargo.toml`, so this is seeded
/// from `std::collections::hash_map::RandomState`, whose per-process seed is
/// itself sourced from OS randomness (it exists precisely to make
/// `HashMap`'s hasher unpredictable). That makes it real-entropy-backed, but
/// it is *not* a documented CSPRNG API — the id is a forensic filename
/// suffix, not a security boundary, so this is judged adequate; flagged here
/// for anyone hardening this further.
fn random_hex_suffix(len_bytes: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = Vec::with_capacity(len_bytes + 8);
    while bytes.len() < len_bytes {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(bytes.len() as u64);
        bytes.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes.truncate(len_bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `base64::decode` with the same strictness as Python's
/// `base64.b64decode(encoded, validate=True)`: reject non-alphabet input
/// instead of silently discarding it, so a corrupt `.enc` can never decode to
/// something that looks like an (empty) valid backup.
fn base64_decode_strict(encoded: &str) -> Result<Vec<u8>, base64::DecodeError> {
    BASE64_STANDARD.decode(encoded)
}

// ---------------------------------------------------------------------------
// Credential vault envelope — real platform-native protection.
// ---------------------------------------------------------------------------
//
// Every per-account backup, `.prev` generation, and unclaimed-credential
// stash entry this store writes to a *file* (as opposed to the macOS
// Keychain, which is already OS-managed protection with no on-disk format of
// its own) is wrapped in a small versioned JSON envelope before it touches
// disk:
//
// ```json
// {"v":1,"scheme":"dpapi"|"plain","data":"<base64>"}
// ```
//
// `scheme` records what protection was *actually* applied to `data` for
// this particular blob, honestly — `"plain"` means exactly what it says:
// `data` is nothing more than base64 of the raw credential bytes, present
// only because JSON strings can't hold arbitrary bytes. `v` is the envelope
// format version, so a future format change can migrate forward instead of
// orphaning old vault files. Both fields, plus the fact that the bytes parse
// as this JSON shape at all, make the envelope self-describing: a reader
// never needs out-of-band knowledge of which build wrote a given file.
//
// Vault files written before this envelope existed are raw base64 with no
// wrapper at all (the exact shape `atomic_b64_write`/`base64_decode_strict`
// used to round-trip). [`unwrap_credential`] accepts both transparently —
// envelope first, legacy raw-base64 as the fallback — and every write path
// upgrades a slot to the envelope the next time it writes, never on a bare
// read. `scheme: "keychain"` is a valid envelope value (see
// [`ProtectionScheme`]) but the file backend never produces it: Keychain
// items are already real OS-level protection and store the raw credential
// string directly, with no envelope needed on that path.

/// The current on-disk envelope format version. Bump this if the envelope
/// shape ever changes, and teach [`unwrap_credential`] to still read the
/// old one.
const CREDENTIAL_ENVELOPE_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct CredentialEnvelope {
    v: u32,
    scheme: String,
    data: String,
}

/// Which real protection this platform/build actually provides for
/// file-backed credential storage right now — exposed so callers (the UI)
/// can report the truth instead of implying "encrypted" where it isn't.
///
/// This reflects the *platform's* native capability, not a specific store's
/// live Keychain-usability state: on macOS the Keychain is the real
/// protection primitive Claude Swap uses for backups whenever it's usable,
/// but a backup that has to fall back to a file (Keychain locked/unusable)
/// still lands in a [`ProtectionScheme::Plain`] envelope — there is no
/// macOS file-encryption primitive available among this crate's
/// dependencies, and claiming otherwise would be dishonest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionScheme {
    /// Windows DPAPI (`CryptProtectData`/`CryptUnprotectData`, per-user):
    /// the ciphertext is useless if copied to another machine or another
    /// user account on the same machine.
    Dpapi,
    /// macOS Keychain (Security.framework), per-user, OS-managed.
    Keychain,
    /// No OS-native protection is available or was applied: base64-in-envelope
    /// with 0600 file permissions and nothing more. This is the honest state
    /// on Linux/WSL (no Secret Service crate in this project's dependencies)
    /// and is also what a Windows/macOS write downgrades to if the native
    /// primitive itself fails.
    Plain,
}

impl ProtectionScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectionScheme::Dpapi => "dpapi",
            ProtectionScheme::Keychain => "keychain",
            ProtectionScheme::Plain => "plain",
        }
    }
}

/// The real protection [`CredentialStore`]'s file backend provides on this
/// platform/build. See [`ProtectionScheme`] for what each value means and
/// why macOS reports `Keychain` here even though a Keychain-unusable
/// fallback write still lands in a `Plain` file envelope.
pub fn protection_scheme() -> ProtectionScheme {
    #[cfg(windows)]
    {
        ProtectionScheme::Dpapi
    }
    #[cfg(target_os = "macos")]
    {
        ProtectionScheme::Keychain
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        ProtectionScheme::Plain
    }
}

/// Windows DPAPI (`CryptProtectData`/`CryptUnprotectData`) bindings, scoped
/// to this module. Import-safe on every platform for the same reason
/// `macos_keychain` is: every real call site is already gated by
/// `cfg(windows)` in [`wrap_platform_bytes`]/[`unwrap_dpapi`], but keeping
/// the module unconditional avoids sprinkling `cfg(windows)` through the
/// envelope logic itself.
///
/// Off Windows the stub `imp` is unreachable by construction, so dead-code
/// lints are allowed there only — Windows still fails on genuinely unused items.
#[cfg_attr(not(windows), allow(dead_code, unused_imports))]
mod dpapi {
    #[derive(Debug)]
    pub struct DpapiError(pub String);

    impl std::fmt::Display for DpapiError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for DpapiError {}

    #[cfg(windows)]
    mod imp {
        use super::DpapiError;
        use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };

        // SAFETY (both functions below): `CryptProtectData`/`CryptUnprotectData`
        // are simple in/out blob transforms — no callback, no window handle
        // (CRYPTPROTECT_UI_FORBIDDEN forbids any UI), no shared state. The
        // input blob points at a Rust-owned buffer that outlives the call; the
        // output blob is zero-initialized and is only populated by the API on
        // success, at which point `pbData` is an OS allocation (via
        // `LocalAlloc` under the hood) that must be released with `LocalFree`
        // — done immediately after copying it into a Rust `Vec` so no raw
        // pointer escapes this function.
        fn call(data: &[u8], protect: bool) -> Result<Vec<u8>, DpapiError> {
            let input = CRYPT_INTEGER_BLOB {
                cbData: data.len() as u32,
                pbData: data.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let ok = unsafe {
                if protect {
                    CryptProtectData(
                        &input,
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        CRYPTPROTECT_UI_FORBIDDEN,
                        &mut output,
                    )
                } else {
                    CryptUnprotectData(
                        &input,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        CRYPTPROTECT_UI_FORBIDDEN,
                        &mut output,
                    )
                }
            };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                return Err(DpapiError(format!(
                    "{} failed: {err}",
                    if protect {
                        "CryptProtectData"
                    } else {
                        "CryptUnprotectData"
                    }
                )));
            }
            // SAFETY: `ok != 0` guarantees the API populated `output` with a
            // valid `pbData`/`cbData` pair before returning.
            let bytes =
                unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }
                    .to_vec();
            // SAFETY: `output.pbData` is the OS allocation from the successful
            // call above, never freed elsewhere, and not used again after
            // this point.
            unsafe {
                LocalFree(output.pbData as HLOCAL);
            }
            Ok(bytes)
        }

        pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, DpapiError> {
            call(plaintext, true)
        }

        pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, DpapiError> {
            call(ciphertext, false)
        }
    }

    #[cfg(not(windows))]
    mod imp {
        use super::DpapiError;

        fn unsupported() -> DpapiError {
            DpapiError("DPAPI is only available on Windows".to_string())
        }

        pub fn protect(_plaintext: &[u8]) -> Result<Vec<u8>, DpapiError> {
            Err(unsupported())
        }
        pub fn unprotect(_ciphertext: &[u8]) -> Result<Vec<u8>, DpapiError> {
            Err(unsupported())
        }
    }

    pub use imp::{protect, unprotect};
}

/// Apply this platform's real file-backend protection to `plaintext`.
/// Infallible by design: a Windows DPAPI failure (rare — no user profile,
/// corrupt master key, …) logs a warning and downgrades *this write only* to
/// a `Plain` envelope rather than failing the write outright. A user's
/// credential vault staying reachable — even under degraded protection, and
/// visibly labeled as such — matters more here than a hard failure; see the
/// module-level safety note on why a write must never be sacrificed for the
/// sake of encryption.
fn wrap_platform_bytes(plaintext: &[u8]) -> (ProtectionScheme, Vec<u8>) {
    #[cfg(windows)]
    {
        match dpapi::protect(plaintext) {
            Ok(ciphertext) => return (ProtectionScheme::Dpapi, ciphertext),
            Err(e) => {
                log::warn!(
                    "DPAPI protect failed, storing this credential as a Plain envelope instead: {e}"
                );
            }
        }
    }
    (ProtectionScheme::Plain, plaintext.to_vec())
}

/// Encode `plaintext` into the versioned on-disk envelope described above.
pub(crate) fn protect_bytes(plaintext: &[u8]) -> Vec<u8> {
    let (scheme, protected) = wrap_platform_bytes(plaintext);
    let envelope = CredentialEnvelope {
        v: CREDENTIAL_ENVELOPE_VERSION,
        scheme: scheme.as_str().to_string(),
        data: BASE64_STANDARD.encode(protected),
    };
    // A struct of plain String fields cannot fail to serialize.
    serde_json::to_vec(&envelope).expect("credential envelope serialization cannot fail")
}

fn wrap_credential(plaintext: &str) -> Vec<u8> {
    protect_bytes(plaintext.as_bytes())
}

pub(crate) fn unprotect_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    let envelope = serde_json::from_slice::<CredentialEnvelope>(raw)
        .map_err(|error| format!("protected artifact is not a valid envelope: {error}"))?;
    if envelope.v != CREDENTIAL_ENVELOPE_VERSION {
        return Err(format!(
            "unsupported protected artifact version {}",
            envelope.v
        ));
    }
    let protected = BASE64_STANDARD
        .decode(&envelope.data)
        .map_err(|error| format!("protected artifact data is not valid base64: {error}"))?;
    match envelope.scheme.as_str() {
        "dpapi" => unwrap_dpapi(&protected),
        "plain" | "keychain" => Ok(protected),
        other => Err(format!("unrecognized protected artifact scheme {other:?}")),
    }
}

/// Decode a stored credential blob, accepting both the current versioned
/// envelope and a legacy raw-base64 blob with no envelope at all (every
/// vault file written before this format existed).
///
/// Detection is envelope-first and structural, not a file-extension or
/// version guess: bytes that parse as `{"v":.., "scheme":.., "data":..}`
/// are treated as the new format (legacy base64 never happens to parse as a
/// JSON object, so there's no ambiguity); anything else falls through to a
/// legacy raw-base64 decode.
///
/// **Never destroys anything.** This function only decodes a byte string
/// already in memory — every caller is responsible for leaving the
/// underlying file/Keychain entry exactly as it was on `Err`; a failed
/// unwrap must never be treated as license to delete or overwrite the
/// stored bytes. Recovery (re-login) is possible only if the bytes survive.
fn unwrap_credential(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();

    if let Ok(envelope) = serde_json::from_str::<CredentialEnvelope>(trimmed) {
        let raw = serde_json::to_vec(&envelope)
            .map_err(|error| format!("could not normalize credential envelope: {error}"))?;
        let plaintext = unprotect_bytes(&raw)?;
        return String::from_utf8(plaintext)
            .map_err(|e| format!("decrypted credential was not valid UTF-8: {e}"));
    }

    // Legacy path: no envelope, just base64 of the raw credential bytes —
    // the only format every vault file used before this change.
    let bytes = base64_decode_strict(trimmed)
        .map_err(|e| format!("not a valid credential envelope or legacy base64 payload: {e}"))?;
    String::from_utf8(bytes)
        .map_err(|e| format!("legacy credential payload was not valid UTF-8: {e}"))
}

#[cfg(windows)]
fn unwrap_dpapi(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    dpapi::unprotect(ciphertext).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn unwrap_dpapi(_ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    Err(
        "this credential was DPAPI-protected on Windows and cannot be decoded on this platform"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// macOS Keychain access.
// ---------------------------------------------------------------------------

/// Generic-password Keychain access, scoped to this module.
///
/// Import-safe on every platform, mirroring the Python `macos_keychain`
/// module's own doc comment: its functions are only meaningful on macOS, and
/// every call site here is already gated by [`Platform::Macos`] /
/// `use_keychain()`, but the module compiles (and simply errors if somehow
/// called) on every target so the rest of `credentials.rs` never needs
/// `#[cfg(target_os = "macos")]` sprinkled through its control flow.
///
/// Unlike the Python version — which deliberately shells out to
/// `/usr/bin/security` so the creator and reader of a Keychain item are
/// always the same stable system binary and macOS never re-prompts for
/// Keychain access after a Python interpreter upgrade — this port talks to
/// Security.framework in-process via the `security-framework` crate (already
/// pinned in `Cargo.toml` for `cfg(target_os = "macos")`). Functionally this
/// reads/writes the exact same generic-password items (same service/account
/// keys), so it is on-disk-compatible with Claude Code and with `cswap`
/// itself; the trade-off is that a rebuilt binary of *this* app is a new
/// Keychain "creator" and macOS may prompt once per rebuild. This could not
/// be verified end-to-end here (no macOS machine, no Rust toolchain in this
/// environment) — see the port report.
mod macos_keychain {
    #[derive(Debug)]
    pub struct KeychainError(pub String);

    impl std::fmt::Display for KeychainError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for KeychainError {}

    /// Account name for the active-credential Keychain item, mirroring
    /// Claude Code's `getUsername()`
    /// (`utils/secureStorage/macOsKeychainHelpers.ts`). `$USER` first, then
    /// the OS username, then a stable final fallback. Matching this exactly
    /// matters on headless/launchd/cron hosts where `$USER` is unset: a
    /// divergent default would key a *different* Keychain item than Claude
    /// Code, so the two could not see each other's active credential.
    pub fn keychain_account_name() -> String {
        if let Ok(user) = std::env::var("USER") {
            if !user.is_empty() {
                return user;
            }
        }
        #[cfg(unix)]
        {
            // SAFETY: geteuid() has no preconditions; getpwuid() with a
            // valid uid returns either a valid pointer into libc's static
            // buffer or NULL, both of which we check before dereferencing.
            unsafe {
                let pw = libc::getpwuid(libc::geteuid());
                if !pw.is_null() {
                    let name = std::ffi::CStr::from_ptr((*pw).pw_name);
                    if let Ok(s) = name.to_str() {
                        if !s.is_empty() {
                            return s.to_string();
                        }
                    }
                }
            }
        }
        "claude-code-user".to_string()
    }

    #[cfg(target_os = "macos")]
    mod imp {
        use super::KeychainError;
        use security_framework::base::Error as SfError;
        use security_framework::passwords::{
            delete_generic_password, get_generic_password, set_generic_password,
        };

        /// The Keychain equivalent of `test_support::guard_real_store`.
        ///
        /// Keychain items are machine-global and keyed by service name, so no
        /// `TempDir` can sandbox them — a test reaching here would read or
        /// overwrite the developer's real Claude Code login. Every test host
        /// pins its platform to the file backend, so this is unreachable; if
        /// it ever is reached, stop rather than touch real credentials.
        #[cfg(test)]
        fn refuse_in_tests(op: &str) -> ! {
            panic!(
                "REFUSING TO RUN: a test reached the real macOS Keychain ({op}).\n\
                 Keychain items are machine-global and cannot be sandboxed by a temp \
                 directory, so this would read or overwrite the real Claude Code login. \
                 Pin the StoreHost's platform to the file backend instead."
            );
        }

        /// `errSecItemNotFound`.
        const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

        fn is_not_found(e: &SfError) -> bool {
            // `code()` is already i32; the cast was a no-op.
            e.code() == ERR_SEC_ITEM_NOT_FOUND
        }

        pub fn get_password(service: &str, account: &str) -> Result<Option<String>, KeychainError> {
            #[cfg(test)]
            refuse_in_tests("get_password");
            match get_generic_password(service, account) {
                Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|e| {
                    KeychainError(format!(
                        "keychain item for {service}/{account} was not valid UTF-8: {e}"
                    ))
                }),
                Err(e) if is_not_found(&e) => Ok(None),
                Err(e) => Err(KeychainError(format!(
                    "keychain find-generic-password failed for {service}/{account}: {e}"
                ))),
            }
        }

        pub fn set_password(
            service: &str,
            account: &str,
            password: &str,
        ) -> Result<(), KeychainError> {
            #[cfg(test)]
            refuse_in_tests("set_password");
            set_generic_password(service, account, password.as_bytes()).map_err(|e| {
                KeychainError(format!(
                    "keychain add-generic-password failed for {service}/{account}: {e}"
                ))
            })
        }

        pub fn delete_password(service: &str, account: &str) -> Result<(), KeychainError> {
            #[cfg(test)]
            refuse_in_tests("delete_password");
            match delete_generic_password(service, account) {
                Ok(()) => Ok(()),
                Err(e) if is_not_found(&e) => Ok(()), // already absent counts as success
                Err(e) => Err(KeychainError(format!(
                    "keychain delete-generic-password failed for {service}/{account}: {e}"
                ))),
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    mod imp {
        use super::KeychainError;

        fn unsupported() -> KeychainError {
            KeychainError("the Keychain is only available on macOS".to_string())
        }

        pub fn get_password(
            _service: &str,
            _account: &str,
        ) -> Result<Option<String>, KeychainError> {
            Err(unsupported())
        }
        pub fn set_password(
            _service: &str,
            _account: &str,
            _password: &str,
        ) -> Result<(), KeychainError> {
            Err(unsupported())
        }
        pub fn delete_password(_service: &str, _account: &str) -> Result<(), KeychainError> {
            Err(unsupported())
        }
    }

    pub use imp::{delete_password, get_password, set_password};
}

use macos_keychain::KeychainError;

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "consumed by the pending switch transaction slice")
)]
fn restore_keychain_entry(
    service: &str,
    account: &str,
    state: &EntryState<String>,
) -> Result<(), CredentialError> {
    match state {
        EntryState::Absent => macos_keychain::delete_password(service, account),
        EntryState::Present(value) => macos_keychain::set_password(service, account, value),
    }
    .map_err(|error| CredentialError::Restore(error.to_string()))
}

#[cfg(target_os = "macos")]
pub(crate) fn recovery_keychain_get(account: &str) -> Result<Option<String>, String> {
    macos_keychain::get_password("cc-logins-switch-recovery", account)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
pub(crate) fn recovery_keychain_set(account: &str, value: &str) -> Result<(), String> {
    macos_keychain::set_password("cc-logins-switch-recovery", account, value)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
pub(crate) fn recovery_keychain_delete(account: &str) -> Result<(), String> {
    macos_keychain::delete_password("cc-logins-switch-recovery", account)
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// CredentialStore.
// ---------------------------------------------------------------------------

/// Owns the active and per-account backup credential stores.
///
/// One store per switcher/session: the Keychain-capability cache is
/// per-process, learned from real Keychain calls, and a fresh process
/// re-evaluates from scratch — mirroring the Python class docstring exactly.
pub struct CredentialStore<H: StoreHost> {
    host: H,
    /// macOS Keychain usability, learned per-process from real Keychain
    /// calls (see `kc_call` / `use_keychain`). `None` = not yet probed;
    /// `Some(true/false)` once an op has run.
    keychain_usable_cache: Option<bool>,
    /// When file mode was entered by a real failure, the instant after which
    /// to re-probe the Keychain. `None` = no pending re-probe (never failed,
    /// or forced to file mode deliberately — see `pin_file_mode`).
    keychain_disabled_until: Option<Instant>,
    last_active_credentials_backend: Option<Backend>,
}

impl<H: StoreHost> CredentialStore<H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            keychain_usable_cache: None,
            keychain_disabled_until: None,
            last_active_credentials_backend: None,
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Where the most recent active-credential write landed, for a
    /// post-switch follow-up message.
    pub fn last_active_credentials_backend(&self) -> Option<Backend> {
        self.last_active_credentials_backend
    }

    // -- Keychain capability cache ------------------------------------------

    /// Run a Keychain operation, learning Keychain usability from the
    /// outcome.
    ///
    /// A success (including "item not found", which the wrapped ops report
    /// as `Ok(None)`, not an error) marks the Keychain usable — but only
    /// flips the cache `None -> Some(true)`, never `Some(false) ->
    /// Some(true)`: once a call has failed this process we stay in file mode
    /// so one process invocation can't split-brain between backends. A
    /// [`KeychainError`] marks it unusable and is returned to the caller to
    /// fall back.
    fn kc_call<T>(
        &mut self,
        op: impl FnOnce() -> Result<T, KeychainError>,
    ) -> Result<T, KeychainError> {
        match op() {
            Ok(value) => {
                if self.keychain_usable_cache.is_none() {
                    self.keychain_usable_cache = Some(true);
                }
                Ok(value)
            }
            Err(e) => {
                self.keychain_usable_cache = Some(false);
                self.keychain_disabled_until = Some(Instant::now() + KEYCHAIN_RECHECK_COOLDOWN);
                Err(e)
            }
        }
    }

    /// Whether credential ops should target the macOS Keychain right now.
    ///
    /// `false` off macOS. On macOS, `true` until a Keychain op fails, which
    /// drops to file mode until the recheck cooldown elapses (self-healing a
    /// transient failure) — unless [`pin_file_mode`](Self::pin_file_mode) was
    /// called, which stays sticky with no cooldown.
    fn use_keychain(&mut self) -> bool {
        if self.host.platform() != Platform::Macos {
            return false;
        }
        if self.keychain_usable_cache == Some(false) {
            if let Some(deadline) = self.keychain_disabled_until {
                if Instant::now() >= deadline {
                    self.keychain_usable_cache = None; // cooldown elapsed -> re-probe
                    self.keychain_disabled_until = None;
                }
            }
        }
        self.keychain_usable_cache != Some(false)
    }

    /// Pin file mode for the rest of the process — no Keychain re-probe.
    ///
    /// A read timeout is safe to recover from (re-probe on cooldown), but an
    /// active-credential *write* that falls back to the file is not: its
    /// best-effort delete of the old Keychain item may have failed, leaving
    /// a stale entry. Re-probing later could read that residual and show the
    /// wrong account, so once a write falls back this process never
    /// re-probes onto a Keychain it could not verify-clear.
    fn pin_file_mode(&mut self) {
        self.keychain_usable_cache = Some(false);
        self.keychain_disabled_until = None;
    }

    // -- active credential: read ---------------------------------------------

    /// Read Claude Code's active credential — OAuth *or* managed API key.
    ///
    /// Thin wrapper over [`read_active_credentials`](Self::read_active_credentials)
    /// preserving the historic `Option<String>` contract: `Some(s)` if
    /// found, `Some(String::new())` if not found, `None` on a file read
    /// error.
    pub fn read_credentials(&mut self) -> Option<String> {
        self.read_active_credentials().value
    }

    /// Read the active OAuth Keychain item with a bounded retry.
    ///
    /// Returns `(value, failed)`. `value` is `Ok`-shaped: `Some(s)` found,
    /// `None` absent. `failed` is true only when *every* attempt raised a
    /// [`KeychainError`] (locked / denied / timeout); a genuinely absent item
    /// is not retried.
    fn read_active_oauth_keychain(&mut self) -> (Option<String>, bool) {
        let mut last_error: Option<KeychainError> = None;
        for attempt in 0..ACTIVE_READ_ATTEMPTS {
            let account = macos_keychain::keychain_account_name();
            match self
                .kc_call(|| macos_keychain::get_password(CLAUDE_CODE_KEYCHAIN_SERVICE, &account))
            {
                Ok(value) => return (value, false),
                Err(e) => {
                    last_error = Some(e);
                    if attempt + 1 < ACTIVE_READ_ATTEMPTS {
                        std::thread::sleep(ACTIVE_READ_RETRY_DELAY);
                    }
                }
            }
        }
        log::warn!(
            "Keychain read failed after {ACTIVE_READ_ATTEMPTS} attempt(s), trying file: {}",
            last_error.map(|e| e.to_string()).unwrap_or_default()
        );
        (None, true)
    }

    /// Read Claude Code's active credential, classifying the outcome.
    ///
    /// Tries the OAuth credential first (Keychain on macOS when usable, with
    /// a bounded retry, then the plaintext `.credentials.json` Claude Code
    /// also falls back to), and only then the managed-key locations (macOS
    /// Keychain, then `~/.claude.json` `primaryApiKey`). Trying OAuth fully
    /// first means a macOS OAuth login that only has a file fallback
    /// (Keychain empty) is never misread as an API key. Non-mutating aside
    /// from the Keychain-capability cache.
    pub fn read_active_credentials(&mut self) -> ActiveCredentials {
        let mut keychain_failed = false;

        // 1. OAuth Keychain (macOS, when usable), with a bounded retry.
        if self.use_keychain() {
            let (val, failed) = self.read_active_oauth_keychain();
            keychain_failed = failed;
            if let Some(v) = val {
                if !v.is_empty() {
                    return ActiveCredentials {
                        value: Some(v),
                        keychain_unavailable: false,
                    };
                }
            }
        } else if self.host.platform() == Platform::Macos {
            // Keychain already known unusable this process: if nothing is
            // found below, that absence is "keychain unavailable", not a
            // genuinely empty slot.
            keychain_failed = true;
        }

        // 2. OAuth plaintext file (Claude Code's own fallback; every platform).
        let cred_file = crate::paths::credentials_path();
        if cred_file.exists() {
            match std::fs::read_to_string(&cred_file) {
                Ok(text) => {
                    if !text.trim().is_empty() {
                        return ActiveCredentials {
                            value: Some(text),
                            keychain_unavailable: false,
                        };
                    }
                }
                Err(e) => {
                    log::error!("Failed to read credentials file: {e}");
                    return ActiveCredentials {
                        value: None,
                        keychain_unavailable: false,
                    };
                }
            }
        }

        // 3. Managed API key (Keychain on macOS, then primaryApiKey).
        let key = self.read_managed_key();
        if !key.is_empty() {
            return ActiveCredentials {
                value: Some(key),
                keychain_unavailable: false,
            };
        }
        // Nothing anywhere. Flag a failed-and-uncovered OAuth Keychain read
        // so the UI distinguishes it from a real empty slot.
        ActiveCredentials {
            value: Some(String::new()),
            keychain_unavailable: keychain_failed,
        }
    }

    /// Snapshot exact presence and bytes for every active credential backend.
    /// Unlike [`Self::read_active_credentials`], this never normalizes absent
    /// and empty values or falls through between backends.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by the pending switch transaction slice")
    )]
    pub(crate) fn snapshot_active_state(
        &mut self,
    ) -> Result<ActiveCredentialState, CredentialError> {
        let file = crate::durable_fs::snapshot(&crate::paths::credentials_path())?;
        let credentials_file = if file.existed {
            EntryState::Present(file.bytes)
        } else {
            EntryState::Absent
        };
        let (oauth_keychain, managed_keychain) = if self.host.platform() == Platform::Macos {
            let account = macos_keychain::keychain_account_name();
            let oauth = macos_keychain::get_password(CLAUDE_CODE_KEYCHAIN_SERVICE, &account)
                .map_err(|error| CredentialError::Snapshot(error.to_string()))?;
            let managed =
                macos_keychain::get_password(CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE, &account)
                    .map_err(|error| CredentialError::Snapshot(error.to_string()))?;
            (
                oauth.map_or(EntryState::Absent, EntryState::Present),
                managed.map_or(EntryState::Absent, EntryState::Present),
            )
        } else {
            (EntryState::Absent, EntryState::Absent)
        };
        Ok(ActiveCredentialState {
            credentials_file,
            oauth_keychain,
            managed_keychain,
        })
    }

    /// Restore every backend exactly, including absence. A failure is
    /// propagated so journal recovery can retain its artifacts and retry.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by the pending switch transaction slice")
    )]
    pub(crate) fn restore_active_state(
        &mut self,
        state: &ActiveCredentialState,
    ) -> Result<(), CredentialError> {
        let file_state = match &state.credentials_file {
            EntryState::Absent => crate::durable_fs::FileState {
                existed: false,
                bytes: Vec::new(),
            },
            EntryState::Present(bytes) => crate::durable_fs::FileState {
                existed: true,
                bytes: bytes.clone(),
            },
        };
        crate::durable_fs::restore(&crate::paths::credentials_path(), &file_state, Some(0o600))
            .map_err(std::io::Error::from)?;

        if self.host.platform() == Platform::Macos {
            let account = macos_keychain::keychain_account_name();
            restore_keychain_entry(
                CLAUDE_CODE_KEYCHAIN_SERVICE,
                &account,
                &state.oauth_keychain,
            )?;
            restore_keychain_entry(
                CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE,
                &account,
                &state.managed_keychain,
            )?;
        } else if state.oauth_keychain != EntryState::Absent
            || state.managed_keychain != EntryState::Absent
        {
            return Err(CredentialError::Restore(
                "snapshot contains Keychain state on a non-macOS backend".to_string(),
            ));
        }
        Ok(())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "consumed by the pending switch transaction slice")
    )]
    pub(crate) fn verify_active_state(
        &mut self,
        expected: &ActiveCredentialState,
    ) -> Result<(), CredentialError> {
        if self.snapshot_active_state()? == *expected {
            Ok(())
        } else {
            Err(CredentialError::VerificationFailed)
        }
    }

    /// Read the active managed API key, or `""` when absent. Non-mutating
    /// aside from the Keychain-capability cache.
    fn read_managed_key(&mut self) -> String {
        if self.use_keychain() {
            let account = macos_keychain::keychain_account_name();
            let val = match self.kc_call(|| {
                macos_keychain::get_password(CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE, &account)
            }) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Managed-key Keychain read failed: {e}");
                    None
                }
            };
            if let Some(v) = val {
                if !v.is_empty() {
                    return v;
                }
            }
        }
        if let Some(cfg) = self.read_global_config() {
            if let Some(Value::String(key)) = cfg.get("primaryApiKey") {
                if !key.is_empty() {
                    return key.clone();
                }
            }
        }
        String::new()
    }

    /// Read and parse `~/.claude.json`, or `None` when absent/unreadable.
    fn read_global_config(&self) -> Option<Map<String, Value>> {
        let path = crate::paths::global_config_path();
        if !path.exists() {
            return None;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("Failed to read global config: {e}");
                return None;
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => Some(map),
            Ok(_) => None,
            Err(e) => {
                log::warn!("Failed to read global config: {e}");
                None
            }
        }
    }

    /// Atomically apply `mutator` to `~/.claude.json`, preserving every key
    /// it doesn't touch (`oauthAccount`, projects, settings, …). 0600
    /// mirrors every other atomic write in this module.
    fn update_global_config(
        &self,
        mutator: impl FnOnce(&mut Map<String, Value>),
    ) -> Result<(), CredentialError> {
        let path = crate::paths::global_config_path();
        let mut data = self.read_global_config().unwrap_or_default();
        mutator(&mut data);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&Value::Object(data))?;
        atomic_write(&path, body.as_bytes())?;
        Ok(())
    }

    // -- active credential: write --------------------------------------------

    /// Atomically write Claude Code's plaintext active-credentials file.
    fn write_active_credentials_file(&self, credentials: &str) -> io::Result<()> {
        let cred_dir = crate::paths::claude_config_home();
        std::fs::create_dir_all(&cred_dir)?;
        atomic_write(&cred_dir.join(".credentials.json"), credentials.as_bytes())
    }

    /// Best-effort removal of the active-credential Keychain item (macOS
    /// only). Claude Code reads the Keychain before the plaintext file, so
    /// once we fall back to the file we must clear any stale Keychain entry
    /// or Claude Code would resurrect it.
    fn delete_active_keychain_entry(&self) {
        if self.host.platform() != Platform::Macos {
            return;
        }
        let account = macos_keychain::keychain_account_name();
        // Best-effort: intentionally not routed through `kc_call` (matches
        // Python calling `macos_keychain.delete_password` directly here) —
        // a down Keychain can't be cleaned right now, and that must not flip
        // the capability cache off the back of a cleanup attempt.
        let _ = macos_keychain::delete_password(CLAUDE_CODE_KEYCHAIN_SERVICE, &account);
    }

    /// Write Claude Code's active credential, enforcing a single auth axis.
    ///
    /// Detects the kind from the payload (raw `sk-ant-api…` key vs OAuth
    /// JSON) and mirrors Claude Code's own `saveApiKey`/`removeApiKey`:
    /// activating one axis clears the other so a stale credential can't
    /// shadow the switch.
    pub fn write_credentials(&mut self, credentials: &str) -> Result<(), CredentialError> {
        if looks_like_api_key(Some(credentials)) {
            self.write_managed_credentials(credentials.trim())
        } else {
            self.write_oauth_credentials(credentials)?;
            self.clear_managed_key();
            Ok(())
        }
    }

    /// Activate a managed API key, then clear OAuth (mutual exclusion).
    ///
    /// Always records `key[-20:]` in `customApiKeyResponses.approved`
    /// (Claude Code does this on every platform, even on Keychain success —
    /// otherwise it re-prompts to approve the key). Stores the key in the
    /// macOS Keychain when usable, else `primaryApiKey`.
    fn write_managed_credentials(&mut self, api_key: &str) -> Result<(), CredentialError> {
        let mut wrote_to_keychain = false;
        if self.use_keychain() {
            let account = macos_keychain::keychain_account_name();
            match self.kc_call(|| {
                macos_keychain::set_password(
                    CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE,
                    &account,
                    api_key,
                )
            }) {
                Ok(()) => wrote_to_keychain = true,
                Err(e) => {
                    log::warn!("Managed-key Keychain write failed, falling back to config: {e}")
                }
            }
        }

        let approved = approved_form(api_key);
        let mutate = |cfg: &mut Map<String, Value>| {
            if !matches!(cfg.get("customApiKeyResponses"), Some(Value::Object(_))) {
                cfg.insert(
                    "customApiKeyResponses".to_string(),
                    Value::Object(Map::new()),
                );
            }
            let responses = cfg
                .get_mut("customApiKeyResponses")
                .and_then(Value::as_object_mut)
                .expect("just inserted or already an object");

            if !matches!(responses.get("approved"), Some(Value::Array(_))) {
                responses.insert("approved".to_string(), Value::Array(Vec::new()));
            }
            let approved_list = responses
                .get_mut("approved")
                .and_then(Value::as_array_mut)
                .expect("just inserted or already an array");
            if !approved_list
                .iter()
                .any(|v| v.as_str() == Some(approved.as_str()))
            {
                approved_list.push(Value::String(approved.clone()));
            }
            responses
                .entry("rejected".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));

            if wrote_to_keychain {
                // Keychain holds the key; keep it out of plaintext config.
                cfg.remove("primaryApiKey");
            } else {
                cfg.insert(
                    "primaryApiKey".to_string(),
                    Value::String(api_key.to_string()),
                );
            }
        };

        self.update_global_config(mutate)
            .map_err(|e| CredentialError::Write(format!("Failed to write managed API key: {e}")))?;

        // Mutual exclusion: drop the OAuth credential so it can't shadow the key.
        self.clear_oauth_credential();
        if self.host.platform() == Platform::Macos && !wrote_to_keychain {
            // Same stale-Keychain resurrection guard as the OAuth path.
            self.pin_file_mode();
        }
        self.last_active_credentials_backend = Some(if wrote_to_keychain {
            Backend::Keychain
        } else {
            Backend::File
        });
        Ok(())
    }

    /// Clear any active managed API key (Claude Code `removeApiKey`
    /// semantics). Leaves `customApiKeyResponses.approved` untouched — Claude
    /// Code's own `removeApiKey` doesn't clear it either.
    fn clear_managed_key(&mut self) {
        if self.host.platform() == Platform::Macos {
            let account = macos_keychain::keychain_account_name();
            // Best-effort, not routed through kc_call — matches Python.
            let _ = macos_keychain::delete_password(CLAUDE_CODE_MANAGED_KEYCHAIN_SERVICE, &account);
        }
        if let Some(cfg) = self.read_global_config() {
            if cfg.get("primaryApiKey").is_some() {
                if let Err(e) = self.update_global_config(|c| {
                    c.remove("primaryApiKey");
                }) {
                    log::warn!("Failed to clear primaryApiKey: {e}");
                }
            }
        }
    }

    /// Clear the active OAuth credential — Keychain item and plaintext file.
    /// Best-effort: a down Keychain or missing file is fine.
    fn clear_oauth_credential(&self) {
        self.delete_active_keychain_entry();
        let cred_file = crate::paths::credentials_path();
        if cred_file.exists() {
            if let Err(e) = std::fs::remove_file(&cred_file) {
                log::warn!("Failed to remove credentials file: {e}");
            }
        }
    }

    /// Write Claude Code's active OAuth credentials.
    ///
    /// macOS writes the Keychain when usable (backend `Keychain`), then
    /// **rewrites an already-present** `.credentials.json` with the same
    /// fresh creds (never *creating* one when absent, never *deleting* one)
    /// so a running Claude Code session's disk-mtime cache invalidation
    /// fires and it hot-reloads instead of serving a memoized token until
    /// restart. If the Keychain write fails — or is already known unusable —
    /// writes the plaintext file and best-effort clears any stale Keychain
    /// entry, recording backend `File`. Linux/WSL/Windows always write the
    /// file.
    fn write_oauth_credentials(&mut self, credentials: &str) -> Result<(), CredentialError> {
        if self.use_keychain() {
            let account = macos_keychain::keychain_account_name();
            match self.kc_call(|| {
                macos_keychain::set_password(CLAUDE_CODE_KEYCHAIN_SERVICE, &account, credentials)
            }) {
                Ok(()) => {
                    self.refresh_stale_credentials_file(credentials);
                    self.last_active_credentials_backend = Some(Backend::Keychain);
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("Keychain write failed, falling back to file: {e}");
                }
            }
        }

        // File mode: non-macOS, macOS Keychain known unusable, or a Keychain
        // write that just failed.
        self.write_active_credentials_file(credentials)
            .map_err(|e| CredentialError::Write(format!("Failed to write credentials: {e}")))?;
        self.delete_active_keychain_entry();
        if self.host.platform() == Platform::Macos {
            // The delete above is best-effort; pin file mode so a later
            // read-timeout cooldown can't re-probe onto a possible residual
            // and resurrect the wrong account.
            self.pin_file_mode();
        }
        self.last_active_credentials_backend = Some(Backend::File);
        Ok(())
    }

    /// Bump an already-present `.credentials.json`'s mtime after a Keychain
    /// write (rewrite-when-present / never-create). Best-effort: the
    /// Keychain write is authoritative on macOS and already succeeded, so a
    /// failure here must not fail the switch.
    fn refresh_stale_credentials_file(&self, credentials: &str) {
        let cred_file = crate::paths::credentials_path();
        if !cred_file.exists() {
            return;
        }
        if let Err(e) = self.write_active_credentials_file(credentials) {
            log::warn!(
                "Could not refresh .credentials.json after Keychain write ({e}); \
                 a running session may not hot-reload until restart"
            );
        }
    }

    // -- per-account backup credentials --------------------------------------
    //
    // Two backends for per-account backups: versioned-envelope `.enc` files
    // (see the envelope doc comment near [`CredentialEnvelope`]) under
    // `credentials_dir` and the macOS Keychain (`SECURITY_SERVICE`). On
    // macOS reads are `.enc`-wins: a fallback `.enc` (written while the
    // Keychain was unusable) is authoritative over a possibly-stale Keychain
    // copy, so a Keychain that recovers can't shadow a newer file. A
    // successful Keychain write therefore reconciles the `.enc` away
    // (correctness-critical, not best-effort).

    /// This host's Keychain service, owned so it can outlive a `&self` borrow
    /// inside a `kc_call` closure.
    fn kc_service(&self) -> String {
        self.host.keychain_service().to_string()
    }

    /// Lowercased for identity keys, because Linux filenames are
    /// case-sensitive and Windows/macOS are not — `User@X.com` and
    /// `user@x.com` were two slots on one platform and one slot (silently
    /// overwriting each other) on the others.
    fn key_email(email: &str) -> String {
        email.trim().to_ascii_lowercase()
    }

    fn backup_enc_path(&self, account_num: &str, email: &str) -> PathBuf {
        let email = Self::key_email(email);
        self.host
            .credentials_dir()
            .join(format!(".creds-{account_num}-{email}.enc"))
    }

    fn backup_username(&self, account_num: &str, email: &str) -> String {
        format!("account-{account_num}-{}", Self::key_email(email))
    }

    /// Read a per-account backup from the Keychain only (no file fallback).
    /// `""` when absent; propagates a real Keychain failure.
    fn kc_read_backup(&mut self, account_num: &str, email: &str) -> Result<String, KeychainError> {
        let account = self.backup_username(account_num, email);
        let service = self.kc_service();
        let creds = self.kc_call(|| macos_keychain::get_password(&service, &account))?;
        Ok(creds.unwrap_or_default())
    }

    fn kc_write_backup(
        &mut self,
        account_num: &str,
        email: &str,
        credentials: &str,
    ) -> Result<(), KeychainError> {
        let account = self.backup_username(account_num, email);
        let service = self.kc_service();
        self.kc_call(|| macos_keychain::set_password(&service, &account, credentials))
    }

    fn kc_delete_backup(&mut self, account_num: &str, email: &str) -> Result<(), KeychainError> {
        let account = self.backup_username(account_num, email);
        let service = self.kc_service();
        self.kc_call(|| macos_keychain::delete_password(&service, &account))
    }

    fn kc_delete_backup_prev(
        &mut self,
        account_num: &str,
        email: &str,
    ) -> Result<(), KeychainError> {
        let account = self.prev_backup_username(account_num, email);
        let service = self.kc_service();
        self.kc_call(|| macos_keychain::delete_password(&service, &account))
    }

    fn delete_backup_keychain_quiet(&mut self, account_num: &str, email: &str) {
        if let Err(e) = self.kc_delete_backup(account_num, email) {
            log::warn!("Failed to delete credentials from Keychain: {e}");
        }
    }

    /// Atomically write a per-account backup `.enc` (versioned envelope) file.
    fn write_backup_enc(
        &self,
        account_num: &str,
        email: &str,
        credentials: &str,
    ) -> io::Result<()> {
        self.atomic_envelope_write(&self.backup_enc_path(account_num, email), credentials)
    }

    /// Atomically write a credential file wrapped in the versioned envelope
    /// (0600), applying whatever real protection this platform's file
    /// backend provides — see the envelope doc comment above
    /// [`CredentialEnvelope`].
    fn atomic_envelope_write(&self, target: &Path, credentials: &str) -> io::Result<()> {
        std::fs::create_dir_all(self.host.credentials_dir())?;
        atomic_write(target, &wrap_credential(credentials))
    }

    /// Stop a leftover `.enc` from shadowing a just-written Keychain backup.
    ///
    /// `.enc`-wins reads make this correctness-critical: delete the `.enc`;
    /// if the delete fails, atomically rewrite it with the same fresh creds;
    /// if that also fails, propagate so the inconsistency surfaces rather
    /// than serving stale (this is the one backup-write step in this module
    /// that is *not* best-effort).
    fn reconcile_enc_after_keychain_write(
        &mut self,
        account_num: &str,
        email: &str,
        credentials: &str,
    ) -> io::Result<()> {
        let enc_file = self.backup_enc_path(account_num, email);
        if !enc_file.exists() {
            return Ok(());
        }
        match std::fs::remove_file(&enc_file) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!(
                    "Could not delete .enc after Keychain backup write ({e}); \
                     rewriting it with the fresh credentials to keep both consistent"
                );
            }
        }
        self.write_backup_enc(account_num, email, credentials)
    }

    /// Read account credentials from backup. `""` when missing.
    ///
    /// macOS is `.enc`-wins (a fallback file beats a possibly-stale Keychain
    /// copy); only an absent or corrupt `.enc` falls through to the
    /// Keychain. Linux/WSL/Windows read the `.enc` only.
    pub fn read_account_credentials(&mut self, account_num: &str, email: &str) -> String {
        let enc_file = self.backup_enc_path(account_num, email);
        let enc_present = match enc_file.try_exists() {
            Ok(present) => present,
            Err(e) => {
                log::warn!("Failed to read credentials file: {e}");
                false
            }
        };

        if enc_present {
            match std::fs::read_to_string(&enc_file) {
                Ok(raw) => match unwrap_credential(&raw) {
                    Ok(decoded) if !decoded.is_empty() => return decoded,
                    Ok(_) => {} // empty/whitespace .enc is not a real backup
                    Err(e) => log::warn!(
                        "Failed to read credentials file (leaving stored bytes untouched): {e}"
                    ),
                },
                Err(e) => log::warn!("Failed to read credentials file: {e}"),
            }
        }

        if self.host.platform() == Platform::Macos {
            match self.kc_read_backup(account_num, email) {
                Ok(v) => return v,
                Err(e) => log::warn!("Failed to read credentials from Keychain: {e}"),
            }
        }
        String::new()
    }

    /// Write account credentials to backup (pure I/O — no session
    /// invalidation).
    ///
    /// macOS writes the Keychain when usable, then reconciles the `.enc`
    /// away. When the Keychain is unusable it writes the `.enc` atomically,
    /// then best-effort deletes any stale Keychain copy so a recovered
    /// Keychain can't shadow the fresh file. Linux/WSL/Windows write the
    /// `.enc` only.
    ///
    /// Before overwriting, the current generation is retained as a `.prev`
    /// file (one generation, best-effort).
    pub fn write_account_credentials(
        &mut self,
        account_num: &str,
        email: &str,
        credentials: &str,
    ) -> Result<(), CredentialError> {
        self.retain_previous_backup(account_num, email, credentials);

        if self.use_keychain() {
            match self.kc_write_backup(account_num, email, credentials) {
                Ok(()) => {
                    return self
                        .reconcile_enc_after_keychain_write(account_num, email, credentials)
                        .map_err(|e| {
                            CredentialError::Write(format!(
                                "failed to reconcile .enc backup after Keychain write for \
                                 slot {account_num} ({email}): {e}"
                            ))
                        });
                }
                Err(e) => {
                    log::warn!("Keychain backup write failed, falling back to file: {e}");
                }
            }
        }

        // File mode: write the .enc atomically, then (macOS) best-effort
        // drop the stale Keychain copy.
        if let Err(e) = self.write_backup_enc(account_num, email, credentials) {
            log::warn!("Failed to write credentials file: {e}");
            return Err(CredentialError::Write(format!(
                "failed to write credentials file: {e}"
            )));
        }
        if self.host.platform() == Platform::Macos {
            self.delete_backup_keychain_quiet(account_num, email);
        }
        Ok(())
    }

    /// Delete account credentials from backup (both backends on macOS,
    /// best-effort). Includes the legacy `account-None-{email}` alias.
    fn delete_account_credentials(&mut self, account_num: &str, email: &str) {
        let mut nums = vec![account_num.to_string()];
        if account_num != "None" {
            nums.push("None".to_string());
        }
        for num in &nums {
            let enc_file = self.backup_enc_path(num, email);
            match enc_file.try_exists() {
                Ok(true) => {
                    if let Err(e) = std::fs::remove_file(&enc_file) {
                        log::warn!("Failed to delete credentials file: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => log::warn!("Failed to delete credentials file: {e}"),
            }
            if self.host.platform() == Platform::Macos {
                self.delete_backup_keychain_quiet(num, email);
            }
            self.delete_previous_backup(num, email);
        }
    }

    /// Clear a slot key, failing closed: error unless emptiness is assured.
    ///
    /// For transactional pre-commit clears (a swap/move write-or-clear step,
    /// or rollback restoration): a destination that must be empty but may
    /// still serve material is exactly the wrong-credential state the
    /// transaction exists to prevent, so backend failures abort the commit
    /// rather than being logged away. Absence itself counts as success on
    /// both backends (missing `.enc`; Keychain "not found"). Legacy-alias and
    /// `.prev` sweeps stay best-effort — reads never serve them.
    pub fn delete_account_credentials_strict(
        &mut self,
        account_num: &str,
        email: &str,
    ) -> Result<(), CredentialError> {
        // Best-effort sweep first: same cruft cleanup a normal delete performs.
        self.delete_account_credentials(account_num, email);

        // Then assure the served key really is gone, propagating failures.
        let enc_path = self.backup_enc_path(account_num, email);
        if let Err(e) = std::fs::remove_file(&enc_path) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(CredentialError::ClearFailed {
                    account_num: account_num.to_string(),
                    email: email.to_string(),
                    reason: format!("aborting before commit: {e}"),
                });
            }
        }
        if self.host.platform() == Platform::Macos {
            if let Err(e) = self.kc_delete_backup(account_num, email) {
                return Err(CredentialError::ClearFailed {
                    account_num: account_num.to_string(),
                    email: email.to_string(),
                    reason: format!("aborting before commit: {e}"),
                });
            }
        }
        // Final belt: catches any backend view the deletes above missed.
        if !self.read_account_credentials(account_num, email).is_empty() {
            return Err(CredentialError::ClearFailed {
                account_num: account_num.to_string(),
                email: email.to_string(),
                reason: "aborting before commit".to_string(),
            });
        }
        Ok(())
    }

    /// Drop a slot key's retained `.prev` generation (both backends).
    /// Best-effort, like retention itself.
    pub fn delete_previous_backup(&mut self, account_num: &str, email: &str) {
        let prev_file = self.prev_backup_path(account_num, email);
        if prev_file.exists() {
            if let Err(e) = std::fs::remove_file(&prev_file) {
                log::warn!("Failed to delete .prev file: {e}");
            }
        }
        if self.host.platform() == Platform::Macos {
            if let Err(e) = self.kc_delete_backup_prev(account_num, email) {
                log::warn!("Failed to delete .prev from Keychain: {e}");
            }
        }
    }

    // -- previous-generation retention ---------------------------------------
    //
    // One retained generation per slot, routed by the same rule as the
    // backup itself: Keychain when the Keychain is in use, `.enc.prev` file
    // otherwise. Retention must not *weaken* the user's storage posture — a
    // Mac whose credentials live in the Keychain must not grow a plaintext
    // copy just for recovery.

    fn prev_backup_path(&self, account_num: &str, email: &str) -> PathBuf {
        let email = Self::key_email(email);
        self.host
            .credentials_dir()
            .join(format!(".creds-{account_num}-{email}.enc.prev"))
    }

    fn prev_backup_username(&self, account_num: &str, email: &str) -> String {
        format!("{}.prev", self.backup_username(account_num, email))
    }

    /// Retain the slot's current backup as `.prev` before it is replaced.
    fn retain_previous_backup(&mut self, account_num: &str, email: &str, new_credentials: &str) {
        let current = self.read_account_credentials(account_num, email);

        // Empty can mean "no backup" or "a backup exists that this platform
        // cannot decode" — a DPAPI envelope read on Linux/macOS. Treating the
        // second as the first destroyed the only copy on the next write, which
        // this module's docs promise cannot happen. Keep the raw bytes.
        if current.is_empty() {
            let path = self.backup_enc_path(account_num, email);
            if path.exists() {
                let prev = self.prev_backup_path(account_num, email);
                if let Err(e) = std::fs::copy(&path, &prev) {
                    log::warn!(
                        "Failed to retain an undecodable backup for account {account_num}: {e}"
                    );
                } else {
                    log::warn!(
                        "Backup for account {account_num} could not be decoded on this platform; \
                         retained the raw bytes as .prev rather than overwriting them"
                    );
                }
            }
            return;
        }

        if current == new_credentials {
            return;
        }
        let result: Result<(), String> = if self.use_keychain() {
            let username = self.prev_backup_username(account_num, email);
            let service = self.kc_service();
            self.kc_call(|| macos_keychain::set_password(&service, &username, &current))
                .map_err(|e| e.to_string())
        } else {
            let path = self.prev_backup_path(account_num, email);
            self.atomic_envelope_write(&path, &current)
                .map_err(|e| e.to_string())
        };
        if let Err(e) = result {
            log::warn!(
                "Failed to retain previous credential generation for account {account_num}: {e}"
            );
        }
    }

    /// Read the retained previous generation. `""` when absent/corrupt.
    /// `.enc.prev`-wins like the main backup read.
    pub fn read_previous_backup(&mut self, account_num: &str, email: &str) -> String {
        let prev_file = self.prev_backup_path(account_num, email);
        if prev_file.exists() {
            match std::fs::read_to_string(&prev_file) {
                Ok(raw) => match unwrap_credential(&raw) {
                    Ok(decoded) if !decoded.is_empty() => return decoded,
                    Ok(_) => {}
                    Err(e) => log::warn!(
                        "Failed to read .prev file (leaving stored bytes untouched): {e}"
                    ),
                },
                Err(e) => log::warn!("Failed to read .prev file: {e}"),
            }
        }
        if self.host.platform() == Platform::Macos {
            let username = self.prev_backup_username(account_num, email);
            let service = self.kc_service();
            match self.kc_call(|| macos_keychain::get_password(&service, &username)) {
                Ok(v) => return v.unwrap_or_default(),
                Err(e) => log::warn!("Failed to read .prev from Keychain: {e}"),
            }
        }
        String::new()
    }

    // -- internal safety copies (unclaimed credentials) ----------------------
    //
    // Write-only preservation for live credential bytes a switch positively
    // attributed to someone other than the outgoing slot. Deliberately 0600
    // files on every platform, unlike the slot backups and `.prev`, which
    // route to the macOS Keychain when it is in use: a failed safety-copy
    // write aborts the switch by design, and that abort path must not
    // inherit the Keychain's failure modes.

    fn stash_manifest_path(&self) -> PathBuf {
        self.host.credentials_dir().join(".unclaimed-manifest.json")
    }

    fn stash_entry_path(&self, entry_id: &str) -> PathBuf {
        self.host
            .credentials_dir()
            .join(format!(".unclaimed-{entry_id}.enc"))
    }

    fn read_stash_manifest(&self) -> Map<String, Value> {
        let path = self.stash_manifest_path();
        if !path.exists() {
            return Map::new();
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("Failed to read unclaimed manifest: {e}");
                return Map::new();
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut data)) => match data.remove("entries") {
                Some(Value::Object(entries)) => entries,
                _ => Map::new(),
            },
            Ok(_) => Map::new(),
            Err(e) => {
                log::warn!("Failed to read unclaimed manifest: {e}");
                Map::new()
            }
        }
    }

    fn write_stash_manifest(&self, entries: Map<String, Value>) -> Result<(), CredentialError> {
        std::fs::create_dir_all(self.host.credentials_dir())?;
        let path = self.stash_manifest_path();

        // A corrupt manifest read as {} must not be silently clobbered — the
        // rows are classification evidence. Set it aside.
        if path.exists() {
            let parses = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .is_some();
            if !parses {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let aside_name = format!(
                    "{}.corrupt-{ts}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("manifest")
                );
                let aside = path.with_file_name(aside_name);
                match std::fs::rename(&path, &aside) {
                    Ok(()) => log::warn!(
                        "Unreadable unclaimed manifest preserved as {}",
                        aside
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or_default()
                    ),
                    Err(e) => log::warn!("Could not preserve corrupt unclaimed manifest: {e}"),
                }
            }
        }

        let mut payload = Map::new();
        payload.insert("schemaVersion".to_string(), Value::from(1));
        payload.insert("entries".to_string(), Value::Object(entries));
        let body = serde_json::to_string_pretty(&Value::Object(payload))?;
        atomic_write(&path, body.as_bytes())?;
        Ok(())
    }

    /// Stash a credential of unknown provenance. Returns the entry id.
    ///
    /// Propagates any failure — callers use a successful stash as the
    /// license to overwrite the live store, so a failed one must be loud.
    /// The entry file is written before the manifest: an entry without
    /// manifest metadata is recoverable; a manifest row without bytes is
    /// not.
    pub fn write_unclaimed_credential(
        &mut self,
        credentials: &str,
        context: Map<String, Value>,
    ) -> Result<String, CredentialError> {
        use sha2::{Digest, Sha256};

        let now = chrono::Utc::now();
        let ts = now.format("%Y%m%dT%H%M%S").to_string();
        let mut hasher = Sha256::new();
        hasher.update(credentials.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        let digest_short = &digest[..12];
        // Nonce keeps ids unique even for identical bytes preserved in the
        // same second — append-only means no write may ever land on an
        // existing id.
        let nonce = random_hex_suffix(3);
        let entry_id = format!("{ts}-{digest_short}-{nonce}");

        self.atomic_envelope_write(&self.stash_entry_path(&entry_id), credentials)?;

        let mut entries = self.read_stash_manifest();
        let mut entry = context;
        entry.insert(
            "createdAt".to_string(),
            Value::String(now.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
        entries.insert(entry_id.clone(), Value::Object(entry));
        self.write_stash_manifest(entries)?;
        Ok(entry_id)
    }

    /// Manifest entries by id, including orphaned entry files (no metadata).
    pub fn list_unclaimed_credentials(&self) -> Map<String, Value> {
        let mut entries = self.read_stash_manifest();
        if let Ok(read_dir) = std::fs::read_dir(self.host.credentials_dir()) {
            for entry in read_dir.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(id) = name
                    .strip_prefix(".unclaimed-")
                    .and_then(|s| s.strip_suffix(".enc"))
                {
                    entries.entry(id.to_string()).or_insert_with(|| {
                        let mut m = Map::new();
                        m.insert("createdAt".to_string(), Value::Null);
                        Value::Object(m)
                    });
                }
            }
        }
        entries
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- pure helpers ---------------------------------------------------------

    #[test]
    fn looks_like_api_key_accepts_bare_managed_keys() {
        assert!(looks_like_api_key(Some("sk-ant-api03-abc123")));
        assert!(looks_like_api_key(Some("  sk-ant-api03-abc123  ")));
    }

    #[test]
    fn looks_like_api_key_rejects_oauth_json_and_setup_tokens() {
        assert!(!looks_like_api_key(None));
        assert!(!looks_like_api_key(Some("")));
        assert!(!looks_like_api_key(Some(
            r#"{"claudeAiOauth": {"accessToken": "sk-ant-api03-abc"}}"#
        )));
        // A garbled setup token must never be misclassified as a managed key.
        assert!(!looks_like_api_key(Some("sk-ant-oat01-abc123")));
    }

    #[test]
    fn shared_credential_fields_extracts_only_the_allowlist() {
        let creds = serde_json::json!({
            "claudeAiOauth": {"accessToken": "tok"},
            "trustedDeviceToken": "dev-tok",
            "mcpOAuth": {"foo": "bar"},
            "pluginSecrets": {"a": 1},
            "someFutureSharedKey": "nope-not-yet-allowlisted"
        })
        .to_string();

        let shared = shared_credential_fields(Some(&creds)).unwrap();
        assert_eq!(shared.len(), 2);
        assert!(shared.contains_key("mcpOAuth"));
        assert!(shared.contains_key("pluginSecrets"));
        assert!(!shared.contains_key("trustedDeviceToken"));
        assert!(!shared.contains_key("someFutureSharedKey"));
    }

    #[test]
    fn shared_credential_fields_none_for_managed_key_or_garbage() {
        assert_eq!(shared_credential_fields(Some("sk-ant-api03-abc")), None);
        assert_eq!(shared_credential_fields(Some("not json")), None);
        assert_eq!(shared_credential_fields(None), None);
        // An empty object is a valid credential object without an OAuth
        // login: no `claudeAiOauth`, so nothing to extract, but the caller
        // still gets a defined (empty) map rather than a JSON-parse failure.
        assert_eq!(shared_credential_fields(Some("{}")), Some(Map::new()));
    }

    #[test]
    fn merge_shared_credential_fields_replaces_only_the_allowlist() {
        let target = serde_json::json!({
            "claudeAiOauth": {"accessToken": "target-tok"},
            "trustedDeviceToken": "target-device",
            "mcpOAuth": {"stale": true}
        })
        .to_string();

        let mut shared = Map::new();
        shared.insert("mcpOAuth".to_string(), serde_json::json!({"fresh": true}));

        let merged = merge_shared_credential_fields(&target, &shared);
        let merged_value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(merged_value["mcpOAuth"], serde_json::json!({"fresh": true}));
        assert_eq!(merged_value["trustedDeviceToken"], "target-device");
        assert_eq!(merged_value["claudeAiOauth"]["accessToken"], "target-tok");
    }

    #[test]
    fn merge_shared_credential_fields_drops_absent_shared_keys() {
        let target = serde_json::json!({
            "claudeAiOauth": {"accessToken": "tok"},
            "mcpOAuth": {"stale": true}
        })
        .to_string();
        // The machine currently holds no shared mcpOAuth generation.
        let shared = Map::new();

        let merged = merge_shared_credential_fields(&target, &shared);
        let merged_value: Value = serde_json::from_str(&merged).unwrap();
        assert!(merged_value.get("mcpOAuth").is_none());
    }

    #[test]
    fn merge_shared_credential_fields_passes_through_managed_keys_verbatim() {
        let target = "sk-ant-api03-abc123";
        let shared = Map::new();
        assert_eq!(merge_shared_credential_fields(target, &shared), target);
    }

    #[test]
    fn approved_form_takes_last_20_chars() {
        let key = "sk-ant-api03-0123456789abcdefghijklmnop";
        let expected: String = key
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(approved_form(key), expected);
        assert_eq!(approved_form(key).chars().count(), 20);
        // Shorter-than-20 keys pass through whole, just trimmed.
        assert_eq!(approved_form("  short-key  "), "short-key");
    }

    // -- CredentialStore: backup credentials (file-backed; no Keychain) ------

    struct TestHost {
        platform: Platform,
        credentials_dir: PathBuf,
    }

    impl StoreHost for TestHost {
        fn platform(&self) -> Platform {
            self.platform
        }
        fn credentials_dir(&self) -> PathBuf {
            self.credentials_dir.clone()
        }
    }

    fn file_backed_store(dir: &Path) -> CredentialStore<TestHost> {
        CredentialStore::new(TestHost {
            // Any non-macOS platform forces the file-only backend, which is
            // exactly what's testable without a real Keychain.
            platform: Platform::Linux,
            credentials_dir: dir.to_path_buf(),
        })
    }

    #[test]
    fn write_then_read_account_credentials_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());

        store
            .write_account_credentials("1", "user@example.com", "the-secret-token")
            .unwrap();

        assert_eq!(
            store.read_account_credentials("1", "user@example.com"),
            "the-secret-token"
        );

        // On-disk format: the versioned envelope, not raw base64.
        let enc_path = dir.path().join(".creds-1-user@example.com.enc");
        let on_disk = std::fs::read_to_string(&enc_path).unwrap();
        let envelope: Value = serde_json::from_str(on_disk.trim()).expect("valid envelope JSON");
        assert_eq!(envelope["v"], 1);
        assert_eq!(envelope["scheme"], expected_file_backend_scheme());
        assert_eq!(
            unwrap_credential(on_disk.trim()).unwrap(),
            "the-secret-token"
        );
    }

    #[test]
    fn read_account_credentials_missing_is_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        assert_eq!(
            store.read_account_credentials("7", "nobody@example.com"),
            ""
        );
    }

    #[test]
    fn corrupt_enc_file_is_treated_as_missing_not_crash() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        std::fs::write(
            dir.path().join(".creds-1-x@example.com.enc"),
            "!!!not-base64!!!",
        )
        .unwrap();
        assert_eq!(store.read_account_credentials("1", "x@example.com"), "");
    }

    // -- credential envelope: format, legacy compat, and decrypt-failure safety --
    //
    // The scheme this process's file backend actually applies:
    // `wrap_platform_bytes` only special-cases `cfg(windows)`, so on macOS a
    // *file*-backed write is always `Plain` (there is no macOS file-crypto
    // primitive among this crate's dependencies) even though
    // `protection_scheme()` reports `Keychain` for the platform overall.
    fn expected_file_backend_scheme() -> &'static str {
        if cfg!(windows) {
            "dpapi"
        } else {
            "plain"
        }
    }

    #[test]
    fn protection_scheme_reports_this_platforms_real_primitive() {
        let expected = if cfg!(windows) {
            ProtectionScheme::Dpapi
        } else if cfg!(target_os = "macos") {
            ProtectionScheme::Keychain
        } else {
            ProtectionScheme::Plain
        };
        assert_eq!(protection_scheme(), expected);
    }

    #[test]
    fn wrap_credential_round_trips_and_is_versioned() {
        let wrapped = wrap_credential("round-trip-secret");
        let text = String::from_utf8(wrapped).unwrap();
        let envelope: Value = serde_json::from_str(&text).expect("wrap_credential emits JSON");
        assert_eq!(envelope["v"], 1);
        assert_eq!(envelope["scheme"], expected_file_backend_scheme());
        assert_eq!(unwrap_credential(&text).unwrap(), "round-trip-secret");
    }

    #[test]
    fn envelope_bytes_are_structurally_detectable_from_legacy_base64() {
        let wrapped = wrap_credential("distinguish-me");
        let wrapped_text = String::from_utf8(wrapped).unwrap();
        // A legacy raw-base64 blob never happens to parse as a JSON object;
        // the new envelope always does. That structural difference is the
        // entire detection mechanism — no separate file marker is needed.
        assert!(serde_json::from_str::<Value>(&wrapped_text)
            .unwrap()
            .is_object());
        let legacy = BASE64_STANDARD.encode(b"distinguish-me");
        assert!(serde_json::from_str::<Value>(&legacy).is_err());
    }

    #[test]
    fn legacy_raw_base64_backup_still_reads_and_upgrades_to_envelope_on_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        let enc_path = dir.path().join(".creds-1-legacy@example.com.enc");
        std::fs::write(&enc_path, BASE64_STANDARD.encode(b"legacy-secret")).unwrap();

        // Reading a pre-envelope vault file works transparently.
        assert_eq!(
            store.read_account_credentials("1", "legacy@example.com"),
            "legacy-secret"
        );

        // The next write upgrades the slot to the envelope format.
        store
            .write_account_credentials("1", "legacy@example.com", "new-secret")
            .unwrap();
        let on_disk = std::fs::read_to_string(&enc_path).unwrap();
        let envelope: Value =
            serde_json::from_str(on_disk.trim()).expect("rewrite upgrades to envelope JSON");
        assert_eq!(envelope["v"], 1);
        assert_eq!(
            store.read_account_credentials("1", "legacy@example.com"),
            "new-secret"
        );
    }

    #[test]
    fn unwrap_credential_rejects_unrecognized_scheme() {
        assert!(unwrap_credential(r#"{"v":1,"scheme":"quantum","data":"AA=="}"#).is_err());
    }

    #[test]
    fn unwrap_credential_accepts_keychain_scheme_as_opaque_bytes() {
        // The file backend never produces this, but a byte stream carrying
        // it (e.g. a future migration) must still round-trip rather than
        // being rejected outright.
        let envelope = format!(
            r#"{{"v":1,"scheme":"keychain","data":"{}"}}"#,
            BASE64_STANDARD.encode(b"already-plaintext-from-keychain")
        );
        assert_eq!(
            unwrap_credential(&envelope).unwrap(),
            "already-plaintext-from-keychain"
        );
    }

    #[test]
    fn truncated_envelope_is_an_error_and_leaves_the_stored_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        let enc_path = dir.path().join(".creds-1-x@example.com.enc");
        // Truncated mid-object: not valid JSON, and not valid base64 either
        // (contains `{`, `"`, `:`), so both decode paths must fail closed.
        let truncated = r#"{"v":1,"scheme":"plain","data":"dGVzdC1z"#;
        std::fs::write(&enc_path, truncated).unwrap();

        assert_eq!(store.read_account_credentials("1", "x@example.com"), "");
        // A failed unwrap must never destroy or rewrite the stored bytes.
        assert_eq!(std::fs::read_to_string(&enc_path).unwrap(), truncated);
    }

    #[test]
    fn envelope_with_invalid_data_field_is_an_error_and_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        let enc_path = dir.path().join(".creds-1-x@example.com.enc");
        let bad = r#"{"v":1,"scheme":"plain","data":"not valid base64 !!"}"#;
        std::fs::write(&enc_path, bad).unwrap();

        assert_eq!(store.read_account_credentials("1", "x@example.com"), "");
        assert_eq!(std::fs::read_to_string(&enc_path).unwrap(), bad);
    }

    #[cfg(unix)]
    #[test]
    fn envelope_file_has_0600_permissions_on_unix_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        store
            .write_account_credentials("1", "perms@example.com", "secret")
            .unwrap();
        let enc_path = dir.path().join(".creds-1-perms@example.com.enc");
        let mode = std::fs::metadata(&enc_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_actually_calls_the_windows_crypto_api() {
        let plaintext = b"dpapi-round-trip-secret";
        let ciphertext = dpapi::protect(plaintext).expect("CryptProtectData should succeed");
        // Sanity that DPAPI actually transformed the bytes rather than
        // passing them through.
        assert_ne!(ciphertext, plaintext);
        let recovered = dpapi::unprotect(&ciphertext).expect("CryptUnprotectData should succeed");
        assert_eq!(recovered, plaintext);
    }

    #[cfg(windows)]
    #[test]
    fn corrupted_dpapi_ciphertext_errors_without_destroying_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        let enc_path = dir.path().join(".creds-1-x@example.com.enc");
        let bogus_ciphertext =
            BASE64_STANDARD.encode(b"not-real-dpapi-ciphertext-bytes-0123456789");
        let envelope = format!(r#"{{"v":1,"scheme":"dpapi","data":"{bogus_ciphertext}"}}"#);
        std::fs::write(&enc_path, &envelope).unwrap();

        assert_eq!(store.read_account_credentials("1", "x@example.com"), "");
        assert_eq!(std::fs::read_to_string(&enc_path).unwrap(), envelope);
    }

    #[test]
    fn second_write_retains_first_generation_as_prev() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());

        store
            .write_account_credentials("1", "a@example.com", "gen-one")
            .unwrap();
        store
            .write_account_credentials("1", "a@example.com", "gen-two")
            .unwrap();

        assert_eq!(
            store.read_account_credentials("1", "a@example.com"),
            "gen-two"
        );
        assert_eq!(store.read_previous_backup("1", "a@example.com"), "gen-one");
    }

    #[test]
    fn writing_identical_credentials_does_not_disturb_prev() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());

        store
            .write_account_credentials("1", "a@example.com", "gen-one")
            .unwrap();
        store
            .write_account_credentials("1", "a@example.com", "gen-two")
            .unwrap();
        // Writing the same value again must not overwrite .prev with gen-two.
        store
            .write_account_credentials("1", "a@example.com", "gen-two")
            .unwrap();

        assert_eq!(store.read_previous_backup("1", "a@example.com"), "gen-one");
    }

    #[test]
    fn delete_account_credentials_strict_clears_enc_and_prev() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());

        store
            .write_account_credentials("1", "a@example.com", "gen-one")
            .unwrap();
        store
            .write_account_credentials("1", "a@example.com", "gen-two")
            .unwrap();
        assert!(!store.read_previous_backup("1", "a@example.com").is_empty());

        store
            .delete_account_credentials_strict("1", "a@example.com")
            .unwrap();

        assert_eq!(store.read_account_credentials("1", "a@example.com"), "");
        assert_eq!(store.read_previous_backup("1", "a@example.com"), "");
    }

    #[test]
    fn delete_account_credentials_strict_is_a_success_when_already_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());
        // Never written — must not error just because there was nothing to clear.
        store
            .delete_account_credentials_strict("9", "ghost@example.com")
            .unwrap();
    }

    #[test]
    fn unclaimed_credential_round_trips_through_manifest_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = file_backed_store(dir.path());

        let mut context = Map::new();
        context.insert(
            "reason".to_string(),
            Value::String("provenance-mismatch".to_string()),
        );

        let entry_id = store
            .write_unclaimed_credential("mystery-token", context)
            .unwrap();

        let entries = store.list_unclaimed_credentials();
        assert!(entries.contains_key(&entry_id));
        assert_eq!(
            entries[&entry_id]["reason"],
            Value::String("provenance-mismatch".to_string())
        );
    }

    #[test]
    fn orphaned_unclaimed_entry_file_is_listed_without_manifest_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = file_backed_store(dir.path());

        let orphan_path = dir.path().join(".unclaimed-orphan-id.enc");
        std::fs::write(&orphan_path, BASE64_STANDARD.encode(b"orphan-bytes")).unwrap();

        let entries = store.list_unclaimed_credentials();
        assert!(entries.contains_key("orphan-id"));
        assert_eq!(entries["orphan-id"]["createdAt"], Value::Null);
    }

    // -- CredentialStore: global-config / active-credential file paths -------
    //
    // These go through `crate::paths`, which (mirroring the Python source)
    // is expected to honor `CLAUDE_CONFIG_DIR`. Env vars are process-global,
    // so these tests are serialized on `crate::test_support::ENV_LOCK` — the
    // single crate-wide lock shared with `paths.rs` and `switcher.rs`. A
    // module-local mutex here would only guard this file's tests against
    // each other while a test in a different module raced past it and
    // mutated the same env var concurrently.

    use crate::test_support::{env_lock, EnvGuard};

    struct EnvVarScope {
        // Declaration order matters: fields drop top-to-bottom, so `_env`
        // (which restores CLAUDE_CONFIG_DIR) must be listed — and therefore
        // drop — before `_lock` (which releases the shared lock). Otherwise
        // another thread could start mutating the env the instant the lock
        // is released, before this scope's own restore has happened.
        _env: EnvGuard,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarScope {
        fn set(dir: &Path) -> Self {
            let lock = env_lock();
            let env = EnvGuard::set("CLAUDE_CONFIG_DIR", dir.to_str().expect("utf8 temp path"));
            Self {
                _env: env,
                _lock: lock,
            }
        }
    }

    #[test]
    fn write_oauth_credentials_writes_plaintext_file_on_non_macos() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());

        let mut store = file_backed_store(dir.path());
        store
            .write_credentials(r#"{"claudeAiOauth": {"accessToken": "tok"}}"#)
            .unwrap();

        let cred_file = dir.path().join(".credentials.json");
        assert!(cred_file.exists());
        let text = std::fs::read_to_string(&cred_file).unwrap();
        assert!(text.contains("claudeAiOauth"));
        assert_eq!(store.last_active_credentials_backend(), Some(Backend::File));
    }

    #[test]
    fn write_managed_credentials_sets_primary_api_key_and_approved() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());

        let mut store = file_backed_store(dir.path());
        store
            .write_credentials("sk-ant-api03-abcdefghijklmnopqrstuvwxyz")
            .unwrap();

        let global_config = dir.path().join(".claude.json");
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(&global_config).unwrap()).unwrap();
        assert_eq!(
            data["primaryApiKey"],
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz"
        );
        let approved = data["customApiKeyResponses"]["approved"]
            .as_array()
            .unwrap();
        assert!(
            approved
                .iter()
                .any(|v| v
                    == &Value::String(approved_form("sk-ant-api03-abcdefghijklmnopqrstuvwxyz")))
        );

        // Mutual exclusion: activating a managed key must clear any OAuth file.
        assert!(!dir.path().join(".credentials.json").exists());
    }

    #[test]
    fn write_credentials_switches_axis_from_oauth_to_managed_key() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());

        let mut store = file_backed_store(dir.path());
        store
            .write_credentials(r#"{"claudeAiOauth": {"accessToken": "tok"}}"#)
            .unwrap();
        assert!(dir.path().join(".credentials.json").exists());

        store
            .write_credentials("sk-ant-api03-abcdefghijklmnopqrstuvwxyz")
            .unwrap();
        assert!(!dir.path().join(".credentials.json").exists());

        let global_config = dir.path().join(".claude.json");
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(&global_config).unwrap()).unwrap();
        assert_eq!(
            data["primaryApiKey"],
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz"
        );
    }

    #[test]
    fn snapshot_distinguishes_absent_from_present_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());
        let mut store = file_backed_store(dir.path());

        let absent = store.snapshot_active_state().unwrap();
        assert_eq!(absent.credentials_file, EntryState::Absent);
        std::fs::write(dir.path().join(".credentials.json"), b"").unwrap();
        let empty = store.snapshot_active_state().unwrap();
        assert_eq!(empty.credentials_file, EntryState::Present(Vec::new()));
    }

    #[test]
    fn restore_reinstates_exact_bytes_and_removes_a_previously_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());
        let mut store = file_backed_store(dir.path());
        let absent = store.snapshot_active_state().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"temporary").unwrap();
        store.restore_active_state(&absent).unwrap();
        assert!(!dir.path().join(".credentials.json").exists());

        let original = b"{\"claudeAiOauth\":{\"accessToken\":\"original\"}}";
        std::fs::write(dir.path().join(".credentials.json"), original).unwrap();
        let snapshot = store.snapshot_active_state().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"changed").unwrap();
        store.restore_active_state(&snapshot).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(".credentials.json")).unwrap(),
            original
        );
        store.verify_active_state(&snapshot).unwrap();
    }

    #[test]
    fn verification_detects_a_stale_shadow_generation() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());
        let mut store = file_backed_store(dir.path());
        std::fs::write(dir.path().join(".credentials.json"), b"first").unwrap();
        let expected = store.snapshot_active_state().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"second").unwrap();
        assert!(matches!(
            store.verify_active_state(&expected),
            Err(CredentialError::VerificationFailed)
        ));
    }

    #[test]
    fn unknown_keychain_presence_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());
        let mut store = CredentialStore::new(TestHost {
            platform: Platform::Macos,
            credentials_dir: dir.path().to_path_buf(),
        });
        assert!(matches!(
            store.snapshot_active_state(),
            Err(CredentialError::Snapshot(_))
        ));
    }

    #[test]
    fn oauth_to_api_key_transition_can_restore_the_exact_oauth_backend() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarScope::set(dir.path());
        let mut store = file_backed_store(dir.path());
        let oauth = r#"{"claudeAiOauth":{"accessToken":"oauth"}}"#;
        store.write_credentials(oauth).unwrap();
        let before = store.snapshot_active_state().unwrap();
        store
            .write_credentials("sk-ant-api03-abcdefghijklmnopqrstuvwxyz")
            .unwrap();
        assert!(!dir.path().join(".credentials.json").exists());
        store.restore_active_state(&before).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".credentials.json")).unwrap(),
            oauth
        );
    }
}
