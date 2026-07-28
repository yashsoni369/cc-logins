//! OAuth token refresh and usage-API client for Claude Code accounts.
//!
//! Ported from **claude-swap** (MIT License), <https://github.com/realiti4/claude-swap>,
//! module `oauth.py` (v0.23.0). This is a behavioural port, not a line-for-line
//! transliteration: Python's `str | None` sentinel values become typed Rust
//! enums here (`RefreshError`, `UsageError` — see their doc comments for the
//! exact mapping table). Every non-obvious branch below exists because of a
//! real bug filed against the upstream project; the comments explain why, not
//! just what.
//!
//! Endpoints:
//! - `GET  {USAGE_URL}`       — `anthropic-beta: oauth-2025-04-20`
//! - `POST {OAUTH_TOKEN_URL}` — `refresh_token` grant
//! - `GET  {PROFILE_URL}`
//!
//! Design: network I/O (`try_refresh_oauth_credentials`, `fetch_oauth_profile`,
//! `fetch_usage`, `try_fetch_usage_for_account`) is kept separate from the pure
//! functions (`normalize_usage_response`, `relevant_windows`, `account_headroom`,
//! `format_reset`, `reset_clock_string`, `credential_fingerprint`,
//! `classify_refresh_failure`, ...) so the latter are unit-testable without a
//! server. See the `tests` module at the bottom.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// Requirement 9: tokens are treated as expired 5 minutes before they actually are.
pub const OAUTH_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;
pub const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

const USER_AGENT: &str = "claude-swap/1.0";

// ---------------------------------------------------------------------------
// Credential parsing (pure)
// ---------------------------------------------------------------------------

/// Extract the OAuth access token from a credentials JSON string.
pub fn extract_access_token(credentials: &str) -> Option<String> {
    let data: Value = serde_json::from_str(credentials).ok()?;
    data.get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(|s| s.to_string())
}

/// Extract the `claudeAiOauth` payload from a credentials JSON string, if it
/// parses and the field is present and is itself an object.
pub fn extract_oauth_data(credentials: &str) -> Option<Map<String, Value>> {
    let data: Value = serde_json::from_str(credentials).ok()?;
    match data.get("claudeAiOauth") {
        Some(Value::Object(map)) => Some(map.clone()),
        _ => None,
    }
}

/// Stable identity fingerprint for a stored credential (requirement 8).
///
/// Refresh-token hash when one exists (survives access-token rotation, so two
/// generations of the same OAuth lineage compare equal); full-content hash
/// otherwise (API keys and setup-tokens rotate never, so content identity is
/// lineage identity). `None` only for empty input — a caller comparing "did
/// the credential change?" must never get `None` for real bytes, or every
/// comparison against it would degenerate to "changed".
pub fn credential_fingerprint(credentials: &str) -> Option<String> {
    if credentials.is_empty() {
        return None;
    }
    let refresh_token = extract_oauth_data(credentials).and_then(|oauth| {
        oauth
            .get("refreshToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    if let Some(token) = refresh_token {
        if !token.is_empty() {
            return Some(format!("sha256:{}", sha256_hex(&token)));
        }
    }
    Some(format!("sha256-full:{}", sha256_hex(credentials)))
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Return whether an OAuth token is expired or about to expire (5-minute
/// buffer, requirement 9). `None` (no `expiresAt`, or not numeric in the
/// source JSON) is treated as "not expired" — matching Python's
/// `isinstance(expires_at, (int, float))` guard.
pub fn is_oauth_token_expired(expires_at: Option<f64>) -> bool {
    let expires_at = match expires_at {
        Some(v) => v,
        None => return false,
    };
    let now_ms = Utc::now().timestamp_millis() as f64;
    now_ms + OAUTH_EXPIRY_BUFFER_MS as f64 >= expires_at
}

/// Short debug summary of stored OAuth token state, e.g.
/// `"oauth: fresh, refresh token yes, expires 20:39 in 3h 12m"`.
pub fn build_token_status(credentials: &str) -> Option<String> {
    let oauth = extract_oauth_data(credentials)?;

    let has_refresh_token = oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let refresh_str = if has_refresh_token { "yes" } else { "no" };

    let expires_at = match oauth.get("expiresAt").and_then(|v| v.as_f64()) {
        Some(v) => v,
        None => {
            return Some(format!(
                "oauth: unknown expiry, refresh token {}",
                refresh_str
            ))
        }
    };

    let expires_utc = DateTime::<Utc>::from_timestamp_millis(expires_at as i64)?;
    let state = if is_oauth_token_expired(Some(expires_at)) {
        "expired"
    } else {
        "fresh"
    };
    let (countdown, clock) = reset_strings(expires_utc);

    Some(format!(
        "oauth: {}, refresh token {}, expires {} in {}",
        state, refresh_str, clock, countdown
    ))
}

// ---------------------------------------------------------------------------
// Reset-time formatting (pure)
// ---------------------------------------------------------------------------

/// Error parsing an ISO-8601 timestamp string (`resets_at` / similar fields).
#[derive(Debug, Clone, PartialEq)]
pub struct TimestampParseError(pub String);

impl fmt::Display for TimestampParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid timestamp: {}", self.0)
    }
}

impl std::error::Error for TimestampParseError {}

fn parse_iso8601(s: &str) -> Result<DateTime<Utc>, TimestampParseError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| TimestampParseError(e.to_string()))
}

/// `(countdown, clock)` for a reset time, e.g. `("3h 12m", "20:39")`.
/// `countdown` and `clock` are always computed against the current instant
/// (`Utc::now()`), matching the Python source — neither this nor
/// `build_token_status` accept an injectable clock.
pub fn format_reset(resets_at: &str) -> Result<(String, String), TimestampParseError> {
    let reset_utc = parse_iso8601(resets_at)?;
    Ok(reset_strings(reset_utc))
}

fn reset_strings(reset_utc: DateTime<Utc>) -> (String, String) {
    let now = Utc::now();
    let remaining_seconds = (reset_utc - now).num_seconds().max(0);

    let days = remaining_seconds / 86_400;
    let rem = remaining_seconds % 86_400;
    let hours = rem / 3_600;
    let minutes = (rem % 3_600) / 60;

    let countdown = if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    };

    (countdown, reset_clock_string(reset_utc, now))
}

/// Absolute reset time in local time: `"20:39"` same local day as `now_utc`,
/// else `"Jul 5 08:59"` (day not zero-padded, matching Python's `str(day)`).
/// Pure and fully deterministic given both instants — unlike [`format_reset`]
/// this takes no implicit `now`, so it is directly unit-testable.
pub fn reset_clock_string(reset_utc: DateTime<Utc>, now_utc: DateTime<Utc>) -> String {
    let reset_local = reset_utc.with_timezone(&Local);
    let now_local = now_utc.with_timezone(&Local);
    if reset_local.date_naive() == now_local.date_naive() {
        reset_local.format("%H:%M").to_string()
    } else {
        let month = reset_local.format("%b").to_string();
        let time = reset_local.format("%H:%M").to_string();
        format!("{} {} {}", month, reset_local.day(), time)
    }
}

/// `(countdown, clock)` for one usage window, or `None` when unknown.
///
/// Recomputed from `resets_at` at render time: strings cached at fetch time
/// drift as the measurement ages. Falls back to the caller-supplied
/// `cached_countdown` / `cached_clock` (persisted alongside a window without a
/// usable `resets_at`) — stale beats blank.
pub fn fresh_reset_strings(
    resets_at: Option<&str>,
    cached_countdown: Option<&str>,
    cached_clock: Option<&str>,
) -> Option<(String, String)> {
    if let Some(resets_at) = resets_at {
        if !resets_at.is_empty() {
            if let Ok(pair) = format_reset(resets_at) {
                return Some(pair);
            }
        }
    }
    cached_clock.map(|clock| (cached_countdown.unwrap_or("?").to_string(), clock.to_string()))
}

// ---------------------------------------------------------------------------
// Usage-response normalisation (pure) — requirements 5, 6, 7
// ---------------------------------------------------------------------------

/// One 5-hour or 7-day utilization window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    pub pct: f64,
    pub resets_at: Option<String>,
    pub countdown: Option<String>,
    pub clock: Option<String>,
}

/// Pay-as-you-go extra-usage spend (requirement 6). A separate axis from the
/// rate-limit windows — deliberately excluded from [`relevant_windows`] /
/// [`account_headroom`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpendEntry {
    /// Dollars/major-currency-units spent (API sends cents; divided by 100).
    pub used: f64,
    pub limit: f64,
    pub pct: f64,
    pub currency: String,
    pub resets_at: Option<String>,
    pub countdown: Option<String>,
    pub clock: Option<String>,
}

/// One per-model weekly window from the newer `limits[]` array (requirement 5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScopedWindow {
    pub name: String,
    pub pct: f64,
    pub resets_at: Option<String>,
    pub countdown: Option<String>,
    pub clock: Option<String>,
}

/// Normalised usage-API response. Mirrors the dict `build_usage_result`
/// returns in the Python source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageResult {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub spend: Option<SpendEntry>,
    pub scoped: Vec<ScopedWindow>,
}

/// Error normalising a raw usage-API response.
///
/// Mirrors the *unguarded* exceptions the Python source lets propagate out of
/// `build_usage_result` for `five_hour` / `seven_day` / `limits[]` entries
/// (a malformed `resets_at` there is not caught locally, unlike inside
/// `extra_usage`; see [`normalize_usage_response`]).
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizeError {
    /// A `resets_at` field on `five_hour`, `seven_day`, or a `limits[]` entry
    /// did not parse. (Python: an uncaught `ValueError` from `format_reset`.)
    Timestamp(String),
    /// `five_hour` / `seven_day` was truthy but had no numeric `utilization`.
    /// (Python: an uncaught `KeyError` indexing `h5["utilization"]`.)
    MissingField(String),
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NormalizeError::Timestamp(msg) => write!(f, "invalid resets_at timestamp: {}", msg),
            NormalizeError::MissingField(field) => write!(f, "missing field: {}", field),
        }
    }
}

impl std::error::Error for NormalizeError {}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn as_finite_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// `five_hour` / `seven_day` → [`Window`]. A missing/non-numeric `utilization`
/// fails the whole normalisation, matching the Python source's unguarded
/// `h5["utilization"]` indexing (an uncaught `KeyError` there propagates all
/// the way out of `build_usage_result`).
fn parse_window(v: &Value) -> Result<Window, NormalizeError> {
    let pct = v
        .get("utilization")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| NormalizeError::MissingField("utilization".to_string()))?;

    let resets_at = v
        .get("resets_at")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty());

    match resets_at {
        Some(ra) => {
            let (countdown, clock) =
                format_reset(ra).map_err(|e| NormalizeError::Timestamp(e.0))?;
            Ok(Window {
                pct,
                resets_at: Some(ra.to_string()),
                countdown: Some(countdown),
                clock: Some(clock),
            })
        }
        None => Ok(Window {
            pct,
            resets_at: None,
            countdown: None,
            clock: None,
        }),
    }
}

/// `extra_usage` → [`SpendEntry`] (requirement 6). `used_credits`,
/// `monthly_limit`, and `utilization` are all nullable: if any is null,
/// missing, or non-numeric, the whole spend entry is skipped — silently,
/// same as the Python source's `except (TypeError, ValueError)` — while the
/// caller keeps whatever `five_hour` / `seven_day` / `scoped` data it already
/// built. A malformed `resets_at` on the spend entry *itself* is caught here
/// too (unlike the top-level windows, see [`parse_window`]) and only drops
/// the spend entry, not the whole response.
fn build_spend_entry(eu: &Value) -> Option<SpendEntry> {
    let used_credits = eu.get("used_credits").filter(|v| !v.is_null());
    let monthly_limit = eu.get("monthly_limit").filter(|v| !v.is_null());
    let utilization = eu.get("utilization").filter(|v| !v.is_null());

    let (used_credits, monthly_limit, utilization) =
        match (used_credits, monthly_limit, utilization) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return None,
        };

    let used_credits = as_finite_f64(used_credits)?;
    let monthly_limit = as_finite_f64(monthly_limit)?;
    let utilization = as_finite_f64(utilization)?;

    let currency = eu
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("USD")
        .to_string();

    let resets_at = eu
        .get("resets_at")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let (resets_at, countdown, clock) = match resets_at {
        Some(ra) => match format_reset(ra) {
            Ok((c, cl)) => (Some(ra.to_string()), Some(c), Some(cl)),
            Err(_) => return None,
        },
        None => (None, None, None),
    };

    Some(SpendEntry {
        used: used_credits / 100.0,
        limit: monthly_limit / 100.0,
        pct: utilization,
        currency,
        resets_at,
        countdown,
        clock,
    })
}

/// Normalise a raw `/api/oauth/usage` response into [`UsageResult`].
///
/// Mirrors `build_usage_result` in the Python source. Handles both the
/// legacy `five_hour` / `seven_day` keys and the newer `limits[]` array
/// (requirement 5): entries there need a non-empty `scope.model.display_name`
/// and numeric `percent`, everything else is skipped; an absent `limits` key
/// simply yields an empty `scoped` list, never an error. Returns `Ok(None)`
/// when the response carries no window data at all.
pub fn normalize_usage_response(data: &Value) -> Result<Option<UsageResult>, NormalizeError> {
    let mut five_hour = None;
    let mut seven_day = None;
    let mut spend = None;
    let mut scoped = Vec::new();

    if let Some(h5) = data.get("five_hour") {
        if truthy(h5) {
            five_hour = Some(parse_window(h5)?);
        }
    }

    if let Some(d7) = data.get("seven_day") {
        if truthy(d7) {
            seven_day = Some(parse_window(d7)?);
        }
    }

    if let Some(eu) = data.get("extra_usage") {
        if truthy(eu) {
            let is_enabled = eu
                .get("is_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_enabled {
                spend = build_spend_entry(eu);
            }
        }
    }

    if let Some(Value::Array(limits)) = data.get("limits") {
        for lim in limits {
            let lim_obj = match lim.as_object() {
                Some(o) => o,
                None => continue,
            };
            let name = lim_obj
                .get("scope")
                .and_then(|s| s.as_object())
                .and_then(|s| s.get("model"))
                .and_then(|m| m.as_object())
                .and_then(|m| m.get("display_name"))
                .and_then(|v| v.as_str());
            let pct = lim_obj.get("percent").and_then(|v| v.as_f64());
            let (name, pct) = match (name, pct) {
                (Some(n), Some(p)) if !n.is_empty() => (n, p),
                _ => continue,
            };
            let resets_at = lim_obj
                .get("resets_at")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let (resets_at, countdown, clock) = match resets_at {
                Some(ra) => {
                    let (c, cl) = format_reset(ra).map_err(|e| NormalizeError::Timestamp(e.0))?;
                    (Some(ra.to_string()), Some(c), Some(cl))
                }
                None => (None, None, None),
            };
            scoped.push(ScopedWindow {
                name: name.to_string(),
                pct,
                resets_at,
                countdown,
                clock,
            });
        }
    }

    let has_any =
        five_hour.is_some() || seven_day.is_some() || spend.is_some() || !scoped.is_empty();
    if has_any {
        Ok(Some(UsageResult {
            five_hour,
            seven_day,
            spend,
            scoped,
        }))
    } else {
        Ok(None)
    }
}

/// Every `(label, pct, resets_at)` window that gates this account.
///
/// Always the 5-hour (`"5h"`) and 7-day (`"7d"`) windows. When `models` is
/// non-empty, each named per-model weekly `scoped` window is included too
/// (matched case-insensitively on display name; the sentinel `"all"` matches
/// every scoped window). `spend` is a separate axis and is deliberately
/// excluded.
pub fn relevant_windows(
    usage: Option<&UsageResult>,
    models: &[&str],
) -> Vec<(String, f64, Option<String>)> {
    let mut windows = Vec::new();
    let usage = match usage {
        Some(u) => u,
        None => return windows,
    };

    if let Some(w) = &usage.five_hour {
        windows.push(("5h".to_string(), w.pct, w.resets_at.clone()));
    }
    if let Some(w) = &usage.seven_day {
        windows.push(("7d".to_string(), w.pct, w.resets_at.clone()));
    }

    if !models.is_empty() {
        let wanted: std::collections::HashSet<String> =
            models.iter().map(|m| m.to_lowercase()).collect();
        let match_all = wanted.contains("all");
        for s in &usage.scoped {
            if match_all || wanted.contains(&s.name.to_lowercase()) {
                windows.push((s.name.clone(), s.pct, s.resets_at.clone()));
            }
        }
    }

    windows
}

/// Remaining percentage before this account hits a rate-limit window
/// (requirement 7). Returns the headroom of the *binding* window
/// (`100 - max(pct)`), so `<= 0` means the account is at or over a limit.
/// Returns `None` when usage is unavailable or carries no window data —
/// callers must treat that as "unknown", never auto-skip the account.
pub fn account_headroom(usage: Option<&UsageResult>, models: &[&str]) -> Option<f64> {
    let pcts: Vec<f64> = relevant_windows(usage, models)
        .into_iter()
        .map(|(_, pct, _)| pct)
        .collect();
    if pcts.is_empty() {
        return None;
    }
    let max_pct = pcts.into_iter().fold(f64::MIN, f64::max);
    Some(100.0 - max_pct)
}

// ---------------------------------------------------------------------------
// Refresh-token grant (network) — requirement 1
// ---------------------------------------------------------------------------

/// Account identity optionally carried alongside a token-endpoint response or
/// an `/api/oauth/profile` fetch. `organization_uuid` and `email` are
/// opportunistic (`str | None` in Python); `uuid` is the only field a caller
/// may rely on, and it is only ever `Some` for a non-empty string.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenAccount {
    pub uuid: String,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

/// Outcome classification for a refresh-token grant attempt.
///
/// Mirrors Python's `RefreshOutcome.error: str | None`:
///
/// | Python sentinel      | Rust variant                   |
/// |-----------------------|----------------------------------|
/// | `None` (success)      | `RefreshOutcome.error = None`   |
/// | `"invalid_grant"`     | `RefreshError::InvalidGrant`    |
/// | `"no_refresh_token"`  | `RefreshError::NoRefreshToken`  |
/// | `"transient"`         | `RefreshError::Transient`       |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshError {
    /// The token endpoint itself rejected the grant (see
    /// [`classify_refresh_failure`]): this refresh token is dead and
    /// re-login is required. Permanent — quarantine, stop retrying.
    InvalidGrant,
    /// The stored credential carries no usable refresh token. Also permanent.
    NoRefreshToken,
    /// Network/server error; the refresh token may still be valid. Retry later.
    Transient,
}

/// Result of a refresh-token grant attempt. `credentials` is the full
/// rotated credentials JSON on success, else `None`.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub credentials: Option<String>,
    pub error: Option<RefreshError>,
    /// Opportunistic identity for the account the rotated token belongs to;
    /// `None` when the server omitted it or on failure.
    pub token_account: Option<TokenAccount>,
}

/// Classify a non-2xx refresh-token response (requirement 1).
///
/// Permanent (`InvalidGrant`) only when the server itself rejected the
/// grant: a 400/401/403 status *and* an explicit `invalid_grant` /
/// `invalid_client` marker in the response body. Everything else — a 400
/// with an unrelated body, or a matching marker riding on an unrelated
/// status like 500 — stays `Transient`. A misclassified transient costs one
/// retry; a misclassified permanent would wrongly quarantine a live token.
pub fn classify_refresh_failure(status: u16, body: &str) -> RefreshError {
    if matches!(status, 400 | 401 | 403)
        && (body.contains("invalid_grant") || body.contains("invalid_client"))
    {
        RefreshError::InvalidGrant
    } else {
        RefreshError::Transient
    }
}

fn parse_token_account(resp_data: &Value) -> Option<TokenAccount> {
    let account = resp_data.get("account")?.as_object()?;
    let uuid = account.get("uuid")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let email = account
        .get("email_address")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let organization_uuid = resp_data
        .get("organization")
        .and_then(|o| o.as_object())
        .and_then(|o| o.get("uuid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(TokenAccount {
        uuid: uuid.to_string(),
        email,
        organization_uuid,
    })
}

/// Refresh an OAuth access token via a direct token-endpoint POST. 10s timeout.
///
/// Never call this for the ACTIVE account's credentials (requirement 2) —
/// that is a caller-side rule (Claude Code owns those bytes), not something
/// this function can enforce, since it has no notion of "active".
pub async fn try_refresh_oauth_credentials(credentials: &str) -> RefreshOutcome {
    let mut data: Value = match serde_json::from_str(credentials) {
        Ok(v) => v,
        Err(_) => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::NoRefreshToken),
                token_account: None,
            }
        }
    };

    let mut oauth: Map<String, Value> = match data.get("claudeAiOauth") {
        Some(Value::Object(map)) => map.clone(),
        _ => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::NoRefreshToken),
                token_account: None,
            }
        }
    };

    let refresh_token = match oauth.get("refreshToken").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::NoRefreshToken),
                token_account: None,
            }
        }
    };

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
    });

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            }
        }
    };

    let send_result = client
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&body)
        .send()
        .await;

    let resp = match send_result {
        Ok(r) => r,
        Err(e) => {
            log::debug!("OAuth refresh failed: {:?}", e);
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            };
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let preview: String = body_text.chars().take(500).collect();
        log::debug!(
            "OAuth refresh failed: HTTP {}, body: {}",
            status.as_u16(),
            preview
        );
        let error = classify_refresh_failure(status.as_u16(), &body_text);
        return RefreshOutcome {
            credentials: None,
            error: Some(error),
            token_account: None,
        };
    }

    let resp_data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            log::debug!("OAuth refresh failed: {:?}", e);
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            };
        }
    };

    let access_token = match resp_data.get("access_token").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            }
        }
    };
    // `.as_f64()` (not `.as_i64()`) so an `expires_in` sent as a JSON float
    // (e.g. `3600.0`) still parses — Python's dynamic typing accepts either.
    let expires_in = match resp_data.get("expires_in").and_then(|v| v.as_f64()) {
        Some(v) => v as i64,
        None => {
            return RefreshOutcome {
                credentials: None,
                error: Some(RefreshError::Transient),
                token_account: None,
            }
        }
    };

    let now_ms = Utc::now().timestamp_millis();
    oauth.insert("accessToken".to_string(), Value::String(access_token));
    oauth.insert(
        "expiresAt".to_string(),
        Value::Number(serde_json::Number::from(now_ms + expires_in * 1000)),
    );
    if let Some(rt) = resp_data.get("refresh_token").and_then(|v| v.as_str()) {
        if !rt.is_empty() {
            oauth.insert("refreshToken".to_string(), Value::String(rt.to_string()));
        }
    }
    if let Some(scope) = resp_data.get("scope").and_then(|v| v.as_str()) {
        if !scope.is_empty() {
            let scopes: Vec<Value> = scope
                .split_whitespace()
                .map(|s| Value::String(s.to_string()))
                .collect();
            oauth.insert("scopes".to_string(), Value::Array(scopes));
        }
    }

    let token_account = parse_token_account(&resp_data);

    if let Value::Object(top) = &mut data {
        top.insert("claudeAiOauth".to_string(), Value::Object(oauth));
    }
    let credentials_out = serde_json::to_string(&data).ok();

    RefreshOutcome {
        credentials: credentials_out,
        error: None,
        token_account,
    }
}

/// Refresh an OAuth access token; `None` on any failure (see [`RefreshOutcome`]
/// for the failure cause).
pub async fn refresh_oauth_credentials(credentials: &str) -> Option<String> {
    try_refresh_oauth_credentials(credentials).await.credentials
}

// ---------------------------------------------------------------------------
// Profile lookup (network)
// ---------------------------------------------------------------------------

/// Resolve an OAuth access token to its account identity, or `None`. 5s
/// timeout. Strictly advisory — callers treat `None` as "unresolvable", never
/// as an error, and must not call this while holding a credential/config lock.
pub async fn fetch_oauth_profile(access_token: &str) -> Option<TokenAccount> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .get(PROFILE_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log::debug!("OAuth profile fetch failed: {:?}", e);
            return None;
        }
    };

    let status = resp.status();
    if !status.is_success() {
        if status.as_u16() == 401 {
            // Evidence, not proof: the live access token can't authenticate.
            // Falls back to pre-fix behavior (proceed without identity).
            log::warn!(
                "OAuth profile returned 401 while resolving credential ownership; \
                 proceeding without identity (pre-fix behavior)."
            );
        } else {
            log::debug!("OAuth profile fetch failed: HTTP {}", status.as_u16());
        }
        return None;
    }

    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            log::debug!("OAuth profile fetch failed: {:?}", e);
            return None;
        }
    };

    let account = data.get("account")?.as_object()?;
    let uuid = account.get("uuid")?.as_str()?.trim();
    if uuid.is_empty() {
        log::debug!("OAuth profile response missing account.uuid");
        return None;
    }
    let email = account
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let organization_uuid = data
        .get("organization")
        .and_then(|o| o.as_object())
        .and_then(|o| o.get("uuid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(TokenAccount {
        uuid: uuid.to_string(),
        email,
        organization_uuid,
    })
}

// ---------------------------------------------------------------------------
// Usage fetch (network) — requirements 3, 4
// ---------------------------------------------------------------------------

/// Internal, unclassified network-layer failure from a usage-API call. Kept
/// private: callers see the classified [`UsageError`] instead.
#[derive(Debug)]
enum UsageFetchError {
    Http { status: u16, retry_after_s: Option<f64> },
    Timeout,
    Network,
    BadResponse,
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    let raw = headers.get("Retry-After")?.to_str().ok()?;
    let secs: f64 = raw.trim().parse().ok()?;
    Some(secs.max(0.0))
}

/// Requirement 4: raw GET against the usage endpoint, 5s timeout. Non-2xx
/// responses carry the parsed `Retry-After` header (seconds form only — the
/// HTTP-date form is rare enough to ignore, matching the Python source).
async fn request_usage_data(access_token: &str) -> Result<Value, UsageFetchError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| UsageFetchError::Network)?;

    let resp = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .header("anthropic-beta", OAUTH_BETA_HEADER)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                UsageFetchError::Timeout
            } else {
                UsageFetchError::Network
            }
        })?;

    let status = resp.status();
    if !status.is_success() {
        let retry_after_s = parse_retry_after(resp.headers());
        return Err(UsageFetchError::Http {
            status: status.as_u16(),
            retry_after_s,
        });
    }

    resp.json::<Value>()
        .await
        .map_err(|_| UsageFetchError::BadResponse)
}

/// Outcome classification for a usage-API fetch attempt.
///
/// Mirrors Python's `UsageOutcome.error: str | None`, built from
/// `_classify_usage_error`'s `kind` (`"http-{code}"`, `"timeout"`,
/// `"network"`, `"bad-response"`, or an exception-type-name fallback) plus
/// the pre-request sentinels added by `try_fetch_usage_for_account`:
///
/// | Python sentinel               | Rust variant                 |
/// |----------------------------------|---------------------------------|
/// | `"no-access-token"`              | `UsageError::NoAccessToken`     |
/// | `"invalid_grant"`                | `UsageError::InvalidGrant`      |
/// | `"refresh-failed"`               | `UsageError::RefreshFailed`     |
/// | `"http-{code}"`                  | `UsageError::Http(code)`        |
/// | `"timeout"`                       | `UsageError::Timeout`           |
/// | `"network"`                       | `UsageError::Network`           |
/// | `"bad-response"`                  | `UsageError::BadResponse`       |
/// | exception-type-name fallback     | `UsageError::Other(String)`     |
#[derive(Debug, Clone, PartialEq)]
pub enum UsageError {
    NoAccessToken,
    InvalidGrant,
    RefreshFailed,
    Http(u16),
    Timeout,
    Network,
    BadResponse,
    Other(String),
}

fn classify_usage_fetch_error(e: &UsageFetchError) -> (UsageError, Option<f64>) {
    match e {
        UsageFetchError::Http {
            status,
            retry_after_s,
        } => (UsageError::Http(*status), *retry_after_s),
        UsageFetchError::Timeout => (UsageError::Timeout, None),
        UsageFetchError::Network => (UsageError::Network, None),
        UsageFetchError::BadResponse => (UsageError::BadResponse, None),
    }
}

fn kind_label(kind: &UsageError) -> String {
    match kind {
        UsageError::Http(code) => format!("http-{}", code),
        UsageError::Timeout => "timeout".to_string(),
        UsageError::Network => "network".to_string(),
        UsageError::BadResponse => "bad-response".to_string(),
        UsageError::Other(s) => s.clone(),
        UsageError::NoAccessToken => "no-access-token".to_string(),
        UsageError::InvalidGrant => "invalid_grant".to_string(),
        UsageError::RefreshFailed => "refresh-failed".to_string(),
    }
}

/// One WARNING-level line with the cause (issue #85 upstream was
/// undiagnosable with failures swallowed at DEBUG). `context` must never
/// carry an email — this is the line users paste into public issues. The
/// server's `Retry-After` rides along when present.
fn log_usage_failure(context: &str, kind: &UsageError, retry_after_s: Option<f64>) {
    let where_str = if context.is_empty() {
        String::new()
    } else {
        format!(" {}", context)
    };
    let mut cause = match retry_after_s {
        Some(s) => format!("{}, retry-after {:.0}s", kind_label(kind), s),
        None => kind_label(kind),
    };
    if matches!(kind, UsageError::Http(429)) {
        // Requirement 4: the endpoint budgets requests per access token;
        // cumulative polling can saturate it. Backoff is the recovery.
        cause.push_str(" (per-token usage budget reached; backing off)");
    }
    log::warn!("Usage fetch failed{}: {}", where_str, cause);
}

/// Fetch usage for a single, already-known-good access token. Never
/// refreshes — callers that need refresh-then-retry semantics use
/// [`try_fetch_usage_for_account`] instead. Errors are logged and folded to
/// `None`, matching the Python source's `fetch_usage`.
pub async fn fetch_usage(access_token: &str) -> Option<UsageResult> {
    match request_usage_data(access_token).await {
        Ok(data) => match normalize_usage_response(&data) {
            Ok(usage) => usage,
            Err(e) => {
                log::warn!("Usage fetch failed: {}", e);
                None
            }
        },
        Err(err) => {
            let (kind, retry_after) = classify_usage_fetch_error(&err);
            log_usage_failure("", &kind, retry_after);
            None
        }
    }
}

/// Result of a usage-API fetch attempt for one account. `usage` can be
/// `Some(None-equivalent)`... concretely, `usage: None` means either failure
/// (`error` set) or a successful round trip whose response carried no window
/// data at all.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageOutcome {
    pub usage: Option<UsageResult>,
    pub error: Option<UsageError>,
    pub retry_after_s: Option<f64>,
}

fn persist(
    callback: Option<&(dyn Fn(&str, &str, &str) -> Result<(), String> + Send + Sync)>,
    account_num: &str,
    email: &str,
    credentials: &str,
) {
    let callback = match callback {
        Some(c) => c,
        None => return,
    };
    if let Err(e) = callback(account_num, email, credentials) {
        // Python additionally calls a CLI `print_warning` here; this port has
        // no printer module (no CLI surface), so the GUI layer is expected to
        // surface persistence failures itself (e.g. a toast) if it cares.
        log::warn!(
            "Refreshed OAuth token for account {} ({}) but failed to persist it: {}. \
             The refresh token on disk may now be stale; if the next refresh fails \
             with invalid_grant, re-run `cswap --add-account` after logging in.",
            account_num,
            email,
            e
        );
    }
}

/// Fetch usage for an account, refreshing an expired token for **inactive**
/// accounts only (requirement 2 — active accounts are never refreshed,
/// Claude Code owns those credentials) and retrying exactly once after a 401
/// for an inactive account (requirement 3).
///
/// `persist_credentials`, when given, is called synchronously with
/// `(account_num, email, rotated_credentials_json)` immediately after any
/// successful refresh, mirroring Python's `persist_credentials` callback.
pub async fn try_fetch_usage_for_account(
    account_num: &str,
    email: &str,
    credentials: &str,
    is_active: bool,
    persist_credentials: Option<&(dyn Fn(&str, &str, &str) -> Result<(), String> + Send + Sync)>,
) -> UsageOutcome {
    // No email in the log context: paste-safe for public issues.
    let context = format!("for account {}", account_num);

    let mut oauth = extract_oauth_data(credentials);
    let mut access_token = match oauth
        .as_ref()
        .and_then(|o| o.get("accessToken"))
        .and_then(|v| v.as_str())
    {
        Some(t) => t.to_string(),
        None => {
            return UsageOutcome {
                usage: None,
                error: Some(UsageError::NoAccessToken),
                retry_after_s: None,
            }
        }
    };

    let mut working_credentials = credentials.to_string();

    let refresh_token_present = oauth
        .as_ref()
        .and_then(|o| o.get("refreshToken"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let expires_at = oauth
        .as_ref()
        .and_then(|o| o.get("expiresAt"))
        .and_then(|v| v.as_f64());

    if !is_active && refresh_token_present && is_oauth_token_expired(expires_at) {
        let refresh = try_refresh_oauth_credentials(&working_credentials).await;
        if let Some(new_creds) = refresh.credentials {
            working_credentials = new_creds;
            persist(persist_credentials, account_num, email, &working_credentials);
            let new_oauth = extract_oauth_data(&working_credentials);
            if let Some(o) = &new_oauth {
                if let Some(t) = o.get("accessToken").and_then(|v| v.as_str()) {
                    access_token = t.to_string();
                }
            }
            if new_oauth.is_some() {
                oauth = new_oauth;
            }
        } else if refresh.error == Some(RefreshError::InvalidGrant) {
            // The refresh-token lineage is server-rejected: permanently
            // dead. Don't hit the usage endpoint with a token we know is
            // expired — report the permanent failure distinctly so the
            // store can quarantine the account.
            return UsageOutcome {
                usage: None,
                error: Some(UsageError::InvalidGrant),
                retry_after_s: None,
            };
        }
        // A transient refresh failure falls through to try the (expired)
        // token; the 401 path below retries the refresh.
    }

    match request_usage_data(&access_token).await {
        Ok(data) => match normalize_usage_response(&data) {
            Ok(usage) => UsageOutcome {
                usage,
                error: None,
                retry_after_s: None,
            },
            Err(e) => UsageOutcome {
                usage: None,
                error: Some(UsageError::Other(e.to_string())),
                retry_after_s: None,
            },
        },
        Err(fetch_err) => {
            let (kind, retry_after) = classify_usage_fetch_error(&fetch_err);
            let is_401 = matches!(fetch_err, UsageFetchError::Http { status: 401, .. });
            let has_refresh = oauth
                .as_ref()
                .and_then(|o| o.get("refreshToken"))
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);

            if !is_401 || is_active || oauth.is_none() || !has_refresh {
                log_usage_failure(&context, &kind, retry_after);
                return UsageOutcome {
                    usage: None,
                    error: Some(kind),
                    retry_after_s: retry_after,
                };
            }

            // Retry once after refreshing on 401 (inactive accounts only,
            // requirement 3). A server-rejected grant means this
            // refresh-token lineage is permanently dead — surface it
            // distinctly (not the generic "refresh-failed") so the store can
            // quarantine instead of retrying a dead token forever.
            let refresh = try_refresh_oauth_credentials(&working_credentials).await;
            let new_creds = match refresh.credentials {
                Some(c) => c,
                None => {
                    log_usage_failure(&context, &kind, None);
                    let dead = refresh.error == Some(RefreshError::InvalidGrant);
                    return UsageOutcome {
                        usage: None,
                        error: Some(if dead {
                            UsageError::InvalidGrant
                        } else {
                            UsageError::RefreshFailed
                        }),
                        retry_after_s: None,
                    };
                }
            };

            working_credentials = new_creds;
            persist(persist_credentials, account_num, email, &working_credentials);
            let refreshed_oauth = extract_oauth_data(&working_credentials);
            let new_token = refreshed_oauth
                .as_ref()
                .and_then(|o| o.get("accessToken"))
                .and_then(|v| v.as_str());
            let new_token = match new_token {
                Some(t) => t.to_string(),
                None => {
                    return UsageOutcome {
                        usage: None,
                        error: Some(UsageError::RefreshFailed),
                        retry_after_s: None,
                    }
                }
            };

            match request_usage_data(&new_token).await {
                Ok(data) => match normalize_usage_response(&data) {
                    Ok(usage) => UsageOutcome {
                        usage,
                        error: None,
                        retry_after_s: None,
                    },
                    Err(e) => UsageOutcome {
                        usage: None,
                        error: Some(UsageError::Other(e.to_string())),
                        retry_after_s: None,
                    },
                },
                Err(retry_err) => {
                    let (kind2, retry_after2) = classify_usage_fetch_error(&retry_err);
                    log_usage_failure(&format!("{} after refresh", context), &kind2, retry_after2);
                    UsageOutcome {
                        usage: None,
                        error: Some(kind2),
                        retry_after_s: retry_after2,
                    }
                }
            }
        }
    }
}

/// Usage dict or `None` (see [`try_fetch_usage_for_account`] for the cause).
pub async fn fetch_usage_for_account(
    account_num: &str,
    email: &str,
    credentials: &str,
    is_active: bool,
    persist_credentials: Option<&(dyn Fn(&str, &str, &str) -> Result<(), String> + Send + Sync)>,
) -> Option<UsageResult> {
    try_fetch_usage_for_account(account_num, email, credentials, is_active, persist_credentials)
        .await
        .usage
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- usage-response normaliser: legacy shape -----------------------------

    #[test]
    fn normalize_legacy_five_hour_seven_day_shape() {
        let data = json!({
            "five_hour": {"utilization": 42.5, "resets_at": "2026-07-28T23:00:00Z"},
            "seven_day": {"utilization": 10.0}
        });
        let result = normalize_usage_response(&data).unwrap().unwrap();

        let h5 = result.five_hour.unwrap();
        assert_eq!(h5.pct, 42.5);
        assert!(h5.resets_at.is_some());
        assert!(h5.countdown.is_some());
        assert!(h5.clock.is_some());

        let d7 = result.seven_day.unwrap();
        assert_eq!(d7.pct, 10.0);
        assert!(d7.resets_at.is_none());
        assert!(d7.countdown.is_none());

        assert!(result.scoped.is_empty());
        assert!(result.spend.is_none());
    }

    #[test]
    fn normalize_empty_five_hour_dict_is_skipped() {
        // Python's `if h5:` is false for an empty dict.
        let data = json!({ "five_hour": {}, "seven_day": {"utilization": 5.0} });
        let result = normalize_usage_response(&data).unwrap().unwrap();
        assert!(result.five_hour.is_none());
        assert_eq!(result.seven_day.unwrap().pct, 5.0);
    }

    #[test]
    fn normalize_missing_utilization_is_an_error() {
        // Mirrors the Python source's uncaught KeyError from h5["utilization"].
        let data = json!({ "five_hour": {"resets_at": "2026-07-28T23:00:00Z"} });
        let err = normalize_usage_response(&data).unwrap_err();
        assert!(matches!(err, NormalizeError::MissingField(_)));
    }

    // -- usage-response normaliser: limits[] shape ---------------------------

    #[test]
    fn normalize_limits_array_shape() {
        let data = json!({
            "limits": [
                {
                    "scope": {"model": {"display_name": "Fable"}},
                    "percent": 55.0,
                    "resets_at": "2026-08-01T00:00:00Z"
                },
                {"scope": {"model": {"display_name": ""}}, "percent": 10.0},
                {"scope": {}, "percent": 5.0},
                {"percent": 5.0},
                "not-an-object"
            ]
        });
        let result = normalize_usage_response(&data).unwrap().unwrap();
        assert_eq!(result.scoped.len(), 1);
        assert_eq!(result.scoped[0].name, "Fable");
        assert_eq!(result.scoped[0].pct, 55.0);
        assert!(result.scoped[0].resets_at.is_some());
    }

    #[test]
    fn normalize_absent_limits_yields_no_scoped_and_is_not_an_error() {
        let data = json!({ "five_hour": {"utilization": 1.0} });
        let result = normalize_usage_response(&data).unwrap().unwrap();
        assert!(result.scoped.is_empty());
    }

    #[test]
    fn normalize_no_window_data_at_all_yields_none() {
        let data = json!({});
        let result = normalize_usage_response(&data).unwrap();
        assert!(result.is_none());
    }

    // -- nullable extra_usage / spend (requirement 6) ------------------------

    #[test]
    fn extra_usage_all_fields_present_yields_spend_in_dollars() {
        let data = json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 1234,
                "monthly_limit": 5000,
                "utilization": 24.68,
                "currency": "USD"
            }
        });
        let result = normalize_usage_response(&data).unwrap().unwrap();
        let spend = result.spend.unwrap();
        assert_eq!(spend.used, 12.34);
        assert_eq!(spend.limit, 50.0);
        assert_eq!(spend.pct, 24.68);
        assert_eq!(spend.currency, "USD");
    }

    #[test]
    fn extra_usage_null_used_credits_skips_only_spend() {
        let data = json!({
            "five_hour": {"utilization": 1.0},
            "extra_usage": {
                "is_enabled": true,
                "used_credits": null,
                "monthly_limit": 5000,
                "utilization": 24.68
            }
        });
        let result = normalize_usage_response(&data).unwrap().unwrap();
        assert!(result.spend.is_none());
        assert!(result.five_hour.is_some());
    }

    #[test]
    fn extra_usage_null_monthly_limit_skips_only_spend() {
        let data = json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 100,
                "monthly_limit": null,
                "utilization": 10.0
            }
        });
        let result = normalize_usage_response(&data).unwrap();
        assert!(result.is_none()); // nothing else present either
    }

    #[test]
    fn extra_usage_not_enabled_yields_no_spend() {
        let data = json!({
            "extra_usage": {
                "is_enabled": false,
                "used_credits": 100,
                "monthly_limit": 5000,
                "utilization": 10.0
            }
        });
        let result = normalize_usage_response(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn extra_usage_missing_utilization_skips_only_spend() {
        let data = json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 100,
                "monthly_limit": 5000
            }
        });
        let result = normalize_usage_response(&data).unwrap();
        assert!(result.is_none());
    }

    // -- headroom math (requirement 7) ---------------------------------------

    #[test]
    fn headroom_is_100_minus_max_binding_window() {
        let usage = UsageResult {
            five_hour: Some(Window {
                pct: 80.0,
                resets_at: None,
                countdown: None,
                clock: None,
            }),
            seven_day: Some(Window {
                pct: 30.0,
                resets_at: None,
                countdown: None,
                clock: None,
            }),
            spend: None,
            scoped: vec![],
        };
        assert_eq!(account_headroom(Some(&usage), &[]), Some(20.0));
    }

    #[test]
    fn headroom_is_none_when_usage_unknown() {
        assert_eq!(account_headroom(None, &[]), None);
    }

    #[test]
    fn headroom_is_none_when_usage_has_no_windows() {
        let usage = UsageResult {
            five_hour: None,
            seven_day: None,
            spend: None,
            scoped: vec![],
        };
        assert_eq!(account_headroom(Some(&usage), &[]), None);
    }

    #[test]
    fn headroom_folds_in_named_scoped_models_only_when_requested() {
        let usage = UsageResult {
            five_hour: Some(Window {
                pct: 10.0,
                resets_at: None,
                countdown: None,
                clock: None,
            }),
            seven_day: None,
            spend: None,
            scoped: vec![ScopedWindow {
                name: "Fable".to_string(),
                pct: 95.0,
                resets_at: None,
                countdown: None,
                clock: None,
            }],
        };
        // Not requested: scoped window ignored, 5h binds.
        assert_eq!(account_headroom(Some(&usage), &[]), Some(90.0));
        // Requested by name (case-insensitive): scoped window binds instead.
        assert_eq!(account_headroom(Some(&usage), &["fable"]), Some(5.0));
        // "all" sentinel matches every scoped window.
        assert_eq!(account_headroom(Some(&usage), &["all"]), Some(5.0));
    }

    // -- error classification -------------------------------------------------

    #[test]
    fn classify_refresh_failure_invalid_grant_requires_status_and_marker() {
        assert_eq!(
            classify_refresh_failure(400, "error: invalid_grant"),
            RefreshError::InvalidGrant
        );
        assert_eq!(
            classify_refresh_failure(401, "{\"error\":\"invalid_client\"}"),
            RefreshError::InvalidGrant
        );
        assert_eq!(
            classify_refresh_failure(403, "invalid_grant"),
            RefreshError::InvalidGrant
        );
    }

    #[test]
    fn classify_refresh_failure_stays_transient_when_ambiguous() {
        // Right status, unrelated body.
        assert_eq!(
            classify_refresh_failure(400, "internal error"),
            RefreshError::Transient
        );
        // Marker present but on a status that doesn't mean "grant rejected".
        assert_eq!(
            classify_refresh_failure(500, "invalid_grant"),
            RefreshError::Transient
        );
        // Neither status nor marker.
        assert_eq!(classify_refresh_failure(503, ""), RefreshError::Transient);
        assert_eq!(classify_refresh_failure(401, ""), RefreshError::Transient);
    }

    #[test]
    fn usage_error_kind_labels_match_python_sentinels() {
        assert_eq!(kind_label(&UsageError::Http(429)), "http-429");
        assert_eq!(kind_label(&UsageError::Timeout), "timeout");
        assert_eq!(kind_label(&UsageError::Network), "network");
        assert_eq!(kind_label(&UsageError::BadResponse), "bad-response");
        assert_eq!(kind_label(&UsageError::InvalidGrant), "invalid_grant");
        assert_eq!(kind_label(&UsageError::RefreshFailed), "refresh-failed");
        assert_eq!(kind_label(&UsageError::NoAccessToken), "no-access-token");
    }

    // -- credential fingerprinting (requirement 8) ---------------------------

    #[test]
    fn credential_fingerprint_empty_input_is_none() {
        assert_eq!(credential_fingerprint(""), None);
    }

    #[test]
    fn credential_fingerprint_prefers_refresh_token_hash() {
        let creds = json!({
            "claudeAiOauth": {"accessToken": "a1", "refreshToken": "r1"}
        })
        .to_string();
        let fp = credential_fingerprint(&creds).unwrap();
        assert!(fp.starts_with("sha256:"));
        assert!(!fp.starts_with("sha256-full:"));

        // Rotating just the access token doesn't change the fingerprint.
        let creds2 = json!({
            "claudeAiOauth": {"accessToken": "a2-rotated", "refreshToken": "r1"}
        })
        .to_string();
        assert_eq!(credential_fingerprint(&creds2), Some(fp));
    }

    #[test]
    fn credential_fingerprint_falls_back_to_full_content_hash() {
        let creds = json!({"claudeAiOauth": {"accessToken": "a1"}}).to_string();
        let fp = credential_fingerprint(&creds).unwrap();
        assert!(fp.starts_with("sha256-full:"));
    }

    // -- token expiry (requirement 9) -----------------------------------------

    #[test]
    fn token_expiry_uses_five_minute_buffer() {
        assert!(!is_oauth_token_expired(None));

        let now_ms = Utc::now().timestamp_millis() as f64;
        assert!(!is_oauth_token_expired(Some(now_ms + 10.0 * 60_000.0))); // 10 min out
        assert!(is_oauth_token_expired(Some(now_ms + 1.0 * 60_000.0))); // 1 min out
        assert!(is_oauth_token_expired(Some(now_ms - 60_000.0))); // already past
    }

    // -- reset-time formatting (pure, deterministic given both instants) -----

    #[test]
    fn reset_clock_string_same_local_day_is_time_only() {
        let now = Utc::now();
        let clock = reset_clock_string(now, now);
        assert_eq!(clock.len(), 5);
        assert!(clock.contains(':'));
    }

    #[test]
    fn format_reset_far_future_yields_day_granularity_countdown() {
        let far_future = Utc::now() + chrono::Duration::days(4) + chrono::Duration::hours(2);
        let (countdown, clock) = format_reset(&far_future.to_rfc3339()).unwrap();
        assert!(countdown.contains('d'));
        assert!(!clock.is_empty());
    }

    #[test]
    fn format_reset_rejects_unparseable_timestamp() {
        assert!(format_reset("not-a-timestamp").is_err());
    }
}
