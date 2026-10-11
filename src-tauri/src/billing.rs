//! Dollar figures for API-key accounts.
//!
//! A Console API key has no 5-hour or weekly quota: it is billed per request
//! from the organisation's credits. What matters is money, so this module
//! keeps, per API account, a day-by-day spend series and turns it into the
//! figures the UI shows (month to date, today, the last 7 days, the balance
//! left).
//!
//! # Where the numbers come from
//!
//! - **Admin key (exact).** With an `sk-ant-admin…` key the organisation's
//!   `GET /v1/organizations/cost_report` is read in daily buckets. That is the
//!   same data the Console's billing page totals, so month to date matches it.
//!   A regular or personal API key cannot read it, and the Console's own
//!   remaining-balance figure has no public endpoint at all, which is why the
//!   balance is entered by the user.
//! - **Estimate (fallback).** Without an Admin key, Claude Code's transcripts
//!   (`<config>/projects/**/*.jsonl`) record the model and token usage of every
//!   reply. Replies timestamped inside the periods this account was seen in
//!   use here are priced with [`price_per_mtok`]. It counts Claude Code
//!   traffic on this machine only, so the UI labels it an estimate. The
//!   transcripts are read incrementally (only bytes appended since the last
//!   read), so the estimate is brought up to date on every snapshot.
//!
//! Nothing here touches quota polling: API accounts are skipped by the usage
//! fetch entirely (see `switcher::read_snapshot_inner`).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Account, Billing, BillingSource, DailySpend, ModelSpend};

const SCHEMA_VERSION: u32 = 1;
/// How often the cost report is re-read, and the transcript index fully
/// re-checked, in the background. The estimate itself also moves on every
/// snapshot (see [`attach`]).
pub const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);
const COST_REPORT_URL: &str = "https://api.anthropic.com/v1/organizations/cost_report";
const USER_AGENT: &str = concat!("cc-logins/", env!("CARGO_PKG_VERSION"));
/// Days of spend kept per account, enough for a balance entered a year ago.
const KEEP_DAYS: i64 = 400;
/// A switch to another account seen within this long of the last sighting
/// is taken to have happened just now: the switch commands publish a
/// snapshot at once, so the previous account was live until then. After a
/// longer gap (cc-logins was not running) the previous period ends at its
/// last sighting instead, so a manual `/login` meanwhile is not counted.
const SPAN_JOIN_GAP_S: i64 = 20 * 60;

// ---------------------------------------------------------------------------
// Admin key storage
// ---------------------------------------------------------------------------

fn billing_dir() -> PathBuf {
    crate::paths::backup_root().join("billing")
}

fn key_file(stable_key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(stable_key.as_bytes());
    billing_dir().join(format!("{}.key", &crate::hex::lower(&digest)[..16]))
}

/// The Admin key saved for this account, if any. Encrypted at rest with the
/// same envelope the vault uses for logins.
pub fn read_admin_key(stable_key: &str) -> Option<String> {
    let raw = std::fs::read(key_file(stable_key)).ok()?;
    let plain = crate::credentials::unprotect_bytes(&raw).ok()?;
    String::from_utf8(plain)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// Save (or, with `None`, forget) this account's Admin key.
pub fn write_admin_key(stable_key: &str, key: Option<&str>) -> Result<(), String> {
    let path = key_file(stable_key);
    match key.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => {
            std::fs::create_dir_all(billing_dir()).map_err(|e| e.to_string())?;
            let protected = crate::credentials::protect_bytes(key.as_bytes());
            crate::durable_fs::stage_sibling(&path, &protected, Some(0o600))
                .map_err(|e| e.to_string())?
                .commit()
                .map_err(|e| e.to_string())?;
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        },
    }
    with_cache(|cache| {
        let row = cache.rows.entry(stable_key.to_string()).or_default();
        row.admin_key_hint = key.map(hint_of);
        // Spend read with a key that is now gone (or replaced) is not
        // re-attributed: the next refresh decides the source again.
        row.error = None;
    });
    Ok(())
}

fn hint_of(key: &str) -> String {
    let chars: Vec<char> = key.trim().chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Span {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    #[serde(default)]
    source: Option<BillingSource>,
    /// UTC day → USD.
    #[serde(default)]
    daily: BTreeMap<String, f64>,
    /// `YYYY-MM` the `by_model` breakdown describes.
    #[serde(default)]
    by_model_month: String,
    #[serde(default)]
    by_model: Vec<ModelSpend>,
    #[serde(default)]
    fetched_at: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    admin_key_hint: Option<String>,
    /// When this account was seen in use here. Drives the estimate only.
    #[serde(default)]
    active_spans: Vec<Span>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Persisted {
    schema_version: u32,
    rows: HashMap<String, Row>,
    #[serde(default)]
    last_live: Option<Sighting>,
}

/// The account last seen live in Claude Code, of any kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Sighting {
    key: String,
    at: DateTime<Utc>,
}

#[derive(Default)]
struct BillingCache {
    rows: HashMap<String, Row>,
    last_live: Option<Sighting>,
}

fn cache_path() -> PathBuf {
    crate::paths::backup_root().join("billing-cache.json")
}

impl BillingCache {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .filter(|p| p.schema_version == SCHEMA_VERSION)
            .map(|p| Self {
                rows: p.rows,
                last_live: p.last_live,
            })
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) {
        let body = Persisted {
            schema_version: SCHEMA_VERSION,
            rows: self.rows.clone(),
            last_live: self.last_live.clone(),
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&body) else {
            return;
        };
        if let Err(e) = crate::durable_fs::stage_sibling(path, &bytes, Some(0o600))
            .and_then(|staged| staged.commit())
        {
            log::debug!("billing cache: could not persist ({e})");
        }
    }
}

/// Serialises every read-modify-write of the cache file within this process:
/// the snapshot path records sightings while the refresher writes spend.
fn cache_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn with_cache<T>(f: impl FnOnce(&mut BillingCache) -> T) -> T {
    let _guard = cache_lock().lock().unwrap_or_else(|p| p.into_inner());
    let path = cache_path();
    let mut cache = BillingCache::load(&path);
    let out = f(&mut cache);
    cache.save(&path);
    out
}

/// Record which account is live in Claude Code right now (`active`: its key
/// and whether it bills from API credits), extending periods of use.
///
/// Accounts only change through cc-logins (or a manual `/login`), so an
/// account seen live twice was live in between, however long the gap: the
/// period runs on. On a switch the previous account's period is extended to
/// now when it was seen recently (see [`SPAN_JOIN_GAP_S`]).
fn note_live(cache: &mut BillingCache, active: Option<(&str, bool)>, now: DateTime<Utc>) {
    let previous = cache.last_live.take();
    let continuing = previous
        .as_ref()
        .is_some_and(|prev| active.is_some_and(|(key, _)| key == prev.key));
    if let Some(prev) = previous.as_ref().filter(|_| !continuing) {
        if (now - prev.at).num_seconds() <= SPAN_JOIN_GAP_S {
            if let Some(span) = cache
                .rows
                .get_mut(&prev.key)
                .and_then(|row| row.active_spans.last_mut())
                .filter(|span| span.to == prev.at)
            {
                span.to = now;
            }
        }
    }
    let Some((key, pay_as_you_go)) = active else {
        return;
    };
    if pay_as_you_go {
        let row = cache.rows.entry(key.to_string()).or_default();
        match row.active_spans.last_mut() {
            Some(span) if continuing => span.to = now,
            // A cache written before `last_live` existed.
            Some(span)
                if previous.is_none() && (now - span.to).num_seconds() <= SPAN_JOIN_GAP_S =>
            {
                span.to = now
            }
            _ => row.active_spans.push(Span { from: now, to: now }),
        }
        let horizon = now - chrono::Duration::days(KEEP_DAYS);
        row.active_spans.retain(|span| span.to >= horizon);
    }
    cache.last_live = Some(Sighting {
        key: key.to_string(),
        at: now,
    });
}

// ---------------------------------------------------------------------------
// The view the UI sees
// ---------------------------------------------------------------------------

fn day_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn month_start(now: DateTime<Utc>) -> NaiveDate {
    NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("day 1 always exists")
}

fn next_month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("midnight on day 1 is unambiguous in UTC")
}

fn sum_since(daily: &BTreeMap<String, f64>, since: NaiveDate) -> f64 {
    daily.range(day_key(since)..).map(|(_, usd)| usd).sum()
}

fn view(row: &Row, account: &Account, has_admin_key: bool, now: DateTime<Utc>) -> Billing {
    let today = now.date_naive();
    let month = month_start(now);
    let limits = &account.billing_limits;
    let daily: Vec<DailySpend> = (0..31)
        .rev()
        .map(|back| today - chrono::Duration::days(back))
        .map(|day| DailySpend {
            usd: row.daily.get(&day_key(day)).copied().unwrap_or(0.0),
            day: day_key(day),
        })
        .collect();
    let balance_left_usd = limits.prepaid_balance_usd.map(|balance| {
        let since = limits
            .balance_set_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc).date_naive())
            .unwrap_or(today);
        balance - sum_since(&row.daily, since)
    });
    let current_month = month.format("%Y-%m").to_string();
    Billing {
        source: row.source,
        month_to_date_usd: sum_since(&row.daily, month),
        today_usd: row.daily.get(&day_key(today)).copied().unwrap_or(0.0),
        last7d_usd: sum_since(&row.daily, today - chrono::Duration::days(6)),
        daily,
        by_model: if row.by_model_month == current_month {
            row.by_model.clone()
        } else {
            Vec::new()
        },
        resets_at: next_month_start(now).to_rfc3339(),
        fetched_at: row.fetched_at.clone(),
        error: row.error.clone(),
        has_admin_key,
        admin_key_hint: if has_admin_key {
            row.admin_key_hint.clone()
        } else {
            None
        },
        monthly_limit_usd: limits.monthly_limit_usd,
        prepaid_balance_usd: limits.prepaid_balance_usd,
        balance_set_at: limits.balance_set_at.clone(),
        balance_left_usd,
        estimated_since: row
            .active_spans
            .first()
            .filter(|_| row.source == Some(BillingSource::Estimate))
            .map(|span| span.from.to_rfc3339()),
    }
}

/// Fill in `billing` on every API account of a freshly read snapshot, and
/// record that the active one is in use right now. No network.
pub fn attach(accounts: &mut [Account]) {
    if !accounts.iter().any(Account::is_pay_as_you_go) {
        return;
    }
    let now = Utc::now();
    let active = accounts
        .iter()
        .find(|a| a.active)
        .map(|a| (a.stable_key(), a.is_pay_as_you_go()));
    with_cache(|cache| {
        note_live(
            cache,
            active.as_ref().map(|(key, payg)| (key.as_str(), *payg)),
            now,
        )
    });
    // Only once the background refresh has built the index: the first full
    // read can take seconds and must not hold up a snapshot. A refresh in
    // progress has the lock, and this snapshot keeps the last figures.
    if INDEX_READY.load(Ordering::Acquire) {
        if let Ok(mut index) = transcripts().try_lock() {
            let keys: Vec<String> = accounts
                .iter()
                .filter(|a| a.is_pay_as_you_go())
                .map(Account::stable_key)
                .filter(|key| !key_file(key).exists())
                .collect();
            update_estimates(&mut index, &keys, now);
        }
    }
    with_cache(|cache| {
        for account in accounts.iter_mut().filter(|a| a.is_pay_as_you_go()) {
            let key = account.stable_key();
            let has_admin_key = key_file(&key).exists();
            let row = cache.rows.entry(key).or_default();
            account.billing = Some(view(row, account, has_admin_key, now));
        }
    });
}

// ---------------------------------------------------------------------------
// Admin API: cost report
// ---------------------------------------------------------------------------

/// `(UTC day, model, USD)`.
type CostRow = (String, String, f64);

/// One page of `cost_report`: `(day, model, usd)` rows and the next cursor.
fn parse_cost_page(body: &Value) -> Result<(Vec<CostRow>, Option<String>), String> {
    let buckets = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("cost report has no data")?;
    let mut rows = Vec::new();
    for bucket in buckets {
        let day = bucket
            .get("starting_at")
            .and_then(Value::as_str)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| day_key(at.with_timezone(&Utc).date_naive()))
            .ok_or("cost report bucket has no start")?;
        for result in bucket
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // A decimal string in cents.
            let cents = match result.get("amount") {
                Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
                Some(Value::Number(n)) => n.as_f64(),
                _ => None,
            };
            let Some(cents) = cents else { continue };
            let model = result
                .get("model")
                .and_then(Value::as_str)
                .or_else(|| result.get("cost_type").and_then(Value::as_str))
                .unwrap_or("other")
                .to_string();
            rows.push((day.clone(), model, cents / 100.0));
        }
    }
    let next = if body.get("has_more").and_then(Value::as_bool) == Some(true) {
        body.get("next_page")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    Ok((rows, next))
}

async fn fetch_cost_report(
    admin_key: &str,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<CostRow>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut page: Option<String> = None;
    // A year of daily buckets is 12 pages of 31; anything past that is a
    // cursor that never ends.
    for _ in 0..16 {
        let mut query = vec![
            ("starting_at", format!("{}T00:00:00Z", day_key(since))),
            ("ending_at", format!("{}T00:00:00Z", day_key(until))),
            ("bucket_width", "1d".to_string()),
            ("group_by[]", "description".to_string()),
            ("limit", "31".to_string()),
        ];
        if let Some(cursor) = &page {
            query.push(("page", cursor.clone()));
        }
        let response = client
            .get(COST_REPORT_URL)
            .query(&query)
            .header("x-api-key", admin_key)
            .header("anthropic-version", "2023-06-01")
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("could not reach Anthropic: {e}"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("unreadable cost report: {e}"))?;
        if !status.is_success() {
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Err(format!("cost report: HTTP {}: {message}", status.as_u16()));
        }
        let (mut page_rows, next) = parse_cost_page(&body)?;
        rows.append(&mut page_rows);
        match next {
            Some(cursor) => page = Some(cursor),
            None => return Ok(rows),
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Estimate from Claude Code's transcripts
// ---------------------------------------------------------------------------

/// `(input, output, cache read)` USD per million tokens. Cache writes are
/// 1.25× input (5-minute) and 2× input (1-hour). Most specific prefix first.
fn price_per_mtok(model: &str) -> Option<(f64, f64, f64)> {
    const TABLE: &[(&str, (f64, f64, f64))] = &[
        ("claude-fable-5-1", (10.0, 50.0, 0.25)),
        ("claude-mythos-5-1", (10.0, 50.0, 0.25)),
        ("claude-fable-5", (10.0, 50.0, 1.0)),
        ("claude-mythos-5", (10.0, 50.0, 1.0)),
        ("claude-opus-5-5", (4.0, 20.0, 0.20)),
        ("claude-opus-5", (5.0, 25.0, 0.50)),
        ("claude-opus-4-8", (5.0, 25.0, 0.50)),
        ("claude-opus-4-7", (5.0, 25.0, 0.50)),
        ("claude-opus-4-6", (5.0, 25.0, 0.50)),
        ("claude-opus-4-5", (5.0, 25.0, 0.50)),
        ("claude-opus-4", (15.0, 75.0, 1.50)),
        ("claude-sonnet-5-5", (2.0, 10.0, 0.20)),
        ("claude-sonnet-5", (2.0, 10.0, 0.20)),
        ("claude-sonnet-4", (3.0, 15.0, 0.30)),
        ("claude-haiku-5-5", (0.10, 0.50, 0.01)),
        ("claude-haiku-4-5", (1.0, 5.0, 0.10)),
        ("claude-3-5-haiku", (0.80, 4.0, 0.08)),
    ];
    let model = model.trim();
    TABLE
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map(|(_, price)| *price)
}

/// USD for one reply's `usage` block, or `None` for an unpriced model.
fn price_usage(model: &str, usage: &Value) -> Option<f64> {
    let (input, output, read) = price_per_mtok(model)?;
    let tokens = |path: &str| usage.pointer(path).and_then(Value::as_f64).unwrap_or(0.0);
    let write_5m = tokens("/cache_creation/ephemeral_5m_input_tokens");
    let write_1h = tokens("/cache_creation/ephemeral_1h_input_tokens");
    let writes = if usage.pointer("/cache_creation").is_some() {
        write_5m * input * 1.25 + write_1h * input * 2.0
    } else {
        tokens("/cache_creation_input_tokens") * input * 1.25
    };
    let usd = (tokens("/input_tokens") * input
        + tokens("/output_tokens") * output
        + tokens("/cache_read_input_tokens") * read
        + writes)
        / 1_000_000.0;
    // Fast mode is billed at twice the standard rate.
    let fast = usage.get("speed").and_then(Value::as_str) == Some("fast");
    Some(if fast { usd * 2.0 } else { usd })
}

fn in_spans(spans: &[Span], at: DateTime<Utc>) -> bool {
    spans.iter().any(|span| at >= span.from && at <= span.to)
}

fn jsonl_files(dir: &Path, modified_since: std::time::SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            jsonl_files(&path, modified_since, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && meta.modified().is_ok_and(|m| m >= modified_since)
        {
            out.push(path);
        }
    }
}

/// One priced reply.
#[derive(Debug, Clone)]
struct Reply {
    at: DateTime<Utc>,
    model: String,
    usd: f64,
}

/// Every priced reply in Claude Code's transcripts since `since`, kept up to
/// date by reading only what was appended to each file since the last scan.
#[derive(Debug, Default)]
struct TranscriptIndex {
    /// Replies older than this were never read.
    since: Option<DateTime<Utc>>,
    /// How far into each transcript has been read: always just after a
    /// newline, so a line still being written is read whole next time.
    read_to: HashMap<PathBuf, u64>,
    /// By request id: a reply with several content blocks is written as
    /// several lines sharing one id, and is counted once.
    replies: HashMap<String, Reply>,
}

impl TranscriptIndex {
    fn scan(&mut self, projects: &Path, since: DateTime<Utc>) {
        if self.since.is_none_or(|indexed| since < indexed) {
            // An earlier start than anything read so far: read it all again.
            *self = Self {
                since: Some(since),
                ..Self::default()
            };
        }
        let since = self.since.unwrap_or(since);
        let mut files = Vec::new();
        jsonl_files(projects, since.into(), &mut files);
        for file in files {
            let offset = self.read_to.get(&file).copied().unwrap_or(0);
            if let Some(read_to) = self.read_file(&file, offset, since) {
                self.read_to.insert(file, read_to);
            }
        }
        let horizon = Utc::now() - chrono::Duration::days(KEEP_DAYS);
        self.replies.retain(|_, reply| reply.at >= horizon);
    }

    /// Read `file` from `offset` (from the start, if it has been rewritten
    /// shorter) to its last complete line. Returns the new offset.
    fn read_file(&mut self, file: &Path, offset: u64, since: DateTime<Utc>) -> Option<u64> {
        let mut handle = std::fs::File::open(file).ok()?;
        let len = handle.metadata().ok()?.len();
        let offset = if len < offset { 0 } else { offset };
        if len == offset {
            return Some(offset);
        }
        handle.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::new();
        handle.read_to_end(&mut bytes).ok()?;
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        for (index, line) in bytes[..complete].split(|b| *b == b'\n').enumerate() {
            let Ok(line) = std::str::from_utf8(line) else {
                continue;
            };
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if entry.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(at) = entry
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
            else {
                continue;
            };
            if at < since {
                continue;
            }
            let message = entry.get("message").unwrap_or(&Value::Null);
            let model = message.get("model").and_then(Value::as_str).unwrap_or("");
            let Some(usd) = message
                .get("usage")
                .and_then(|usage| price_usage(model, usage))
            else {
                continue;
            };
            let id = entry
                .get("requestId")
                .and_then(Value::as_str)
                .or_else(|| message.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}#{offset}:{index}", file.display()));
            // Later lines of one request repeat its usage with the output
            // grown so far: keep the final, largest one.
            let reply = Reply {
                at,
                model: model.to_string(),
                usd,
            };
            match self.replies.entry(id) {
                std::collections::hash_map::Entry::Occupied(mut seen) => {
                    if usd > seen.get().usd {
                        seen.insert(reply);
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(reply);
                }
            }
        }
        Some(offset + complete as u64)
    }

    /// Spend per day and per `(month, model)` for replies inside `spans`.
    fn tally(&self, spans: &[Span]) -> (BTreeMap<String, f64>, HashMap<(String, String), f64>) {
        let mut daily = BTreeMap::new();
        let mut by_model: HashMap<(String, String), f64> = HashMap::new();
        for reply in self.replies.values().filter(|r| in_spans(spans, r.at)) {
            let day = day_key(reply.at.date_naive());
            *daily.entry(day.clone()).or_insert(0.0) += reply.usd;
            *by_model
                .entry((day[..7].to_string(), reply.model.clone()))
                .or_insert(0.0) += reply.usd;
        }
        (daily, by_model)
    }
}

/// Spend per day and per model for replies inside `spans`, read from the
/// transcripts under `projects` (incrementally, through `index`).
fn estimate(
    index: &mut TranscriptIndex,
    projects: &Path,
    spans: &[Span],
) -> (BTreeMap<String, f64>, HashMap<(String, String), f64>) {
    let Some(earliest) = spans.iter().map(|span| span.from).min() else {
        return (BTreeMap::new(), HashMap::new());
    };
    index.scan(projects, earliest);
    index.tally(spans)
}

fn transcripts() -> &'static Mutex<TranscriptIndex> {
    static INDEX: OnceLock<Mutex<TranscriptIndex>> = OnceLock::new();
    INDEX.get_or_init(|| Mutex::new(TranscriptIndex::default()))
}

/// Set once a background refresh has read the transcripts in full, after
/// which each snapshot only reads what was appended.
static INDEX_READY: AtomicBool = AtomicBool::new(false);

/// Bring the estimate of these accounts up to date. The cache lock is not
/// held while the transcripts are read.
fn update_estimates(index: &mut TranscriptIndex, keys: &[String], now: DateTime<Utc>) {
    if keys.is_empty() {
        return;
    }
    let spans: Vec<(String, Vec<Span>)> = with_cache(|cache| {
        keys.iter()
            .map(|key| {
                let spans = cache
                    .rows
                    .get(key)
                    .map(|row| row.active_spans.clone())
                    .unwrap_or_default();
                (key.clone(), spans)
            })
            .collect()
    });
    let projects = crate::paths::claude_config_home().join("projects");
    let estimates: Vec<_> = spans
        .iter()
        .map(|(key, spans)| (key, estimate(index, &projects, spans)))
        .collect();
    let today = now.date_naive();
    let month = month_start(now).format("%Y-%m").to_string();
    with_cache(|cache| {
        for (key, (daily, by_model)) in estimates {
            let row = cache.rows.entry(key.clone()).or_default();
            row.daily = daily;
            row.by_model = month_breakdown(
                by_model
                    .into_iter()
                    .map(|((month, model), usd)| (month, model, usd)),
                &month,
            );
            row.by_model_month = month.clone();
            row.source = Some(BillingSource::Estimate);
            row.fetched_at = Some(now.to_rfc3339());
            row.error = None;
            prune(row, today);
        }
    });
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

fn month_breakdown(rows: impl IntoIterator<Item = CostRow>, month: &str) -> Vec<ModelSpend> {
    let mut totals: HashMap<String, f64> = HashMap::new();
    for (day, model, usd) in rows {
        if day.starts_with(month) {
            *totals.entry(model).or_insert(0.0) += usd;
        }
    }
    let mut out: Vec<ModelSpend> = totals
        .into_iter()
        .filter(|(_, usd)| *usd > 0.0)
        .map(|(model, usd)| ModelSpend { model, usd })
        .collect();
    out.sort_by(|a, b| b.usd.total_cmp(&a.usd));
    out
}

/// Re-read spend for every API account: the cost report where an Admin key is
/// saved, the transcript estimate otherwise. Errors stay on the account (the
/// last figures are kept) rather than failing the whole refresh.
pub async fn refresh(accounts: &[Account]) {
    let now = Utc::now();
    let today = now.date_naive();
    let month = month_start(now).format("%Y-%m").to_string();
    let mut estimated = Vec::new();
    for account in accounts.iter().filter(|a| a.is_pay_as_you_go()) {
        let key = account.stable_key();
        match read_admin_key(&key) {
            Some(admin_key) => {
                let balance_day = account
                    .billing_limits
                    .balance_set_at
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc).date_naive());
                let since = [
                    Some(month_start(now)),
                    Some(today - chrono::Duration::days(30)),
                    balance_day,
                ]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(today)
                .max(today - chrono::Duration::days(365));
                let result =
                    fetch_cost_report(&admin_key, since, today + chrono::Duration::days(1)).await;
                with_cache(|cache| {
                    let row = cache.rows.entry(key.clone()).or_default();
                    match result {
                        Ok(rows) => {
                            let mut fetched: BTreeMap<String, f64> = BTreeMap::new();
                            for (day, _, usd) in &rows {
                                *fetched.entry(day.clone()).or_insert(0.0) += usd;
                            }
                            // The report covers every day from `since`, so
                            // days with no spend are zero, not missing.
                            row.daily
                                .retain(|day, _| day.as_str() < day_key(since).as_str());
                            row.daily.extend(fetched);
                            row.by_model = month_breakdown(rows, &month);
                            row.by_model_month = month.clone();
                            row.source = Some(BillingSource::AdminApi);
                            row.fetched_at = Some(now.to_rfc3339());
                            row.error = None;
                        }
                        Err(error) => {
                            log::warn!("account {}: {error}", account.number);
                            row.error = Some(error);
                        }
                    }
                    prune(row, today);
                });
            }
            None => estimated.push(key),
        }
    }
    let ready = tokio::task::spawn_blocking(move || {
        let mut index = transcripts().lock().unwrap_or_else(|p| p.into_inner());
        update_estimates(&mut index, &estimated, now);
    })
    .await;
    if ready.is_ok() {
        INDEX_READY.store(true, Ordering::Release);
    }
}

fn prune(row: &mut Row, today: NaiveDate) {
    let horizon = day_key(today - chrono::Duration::days(KEEP_DAYS));
    row.daily.retain(|day, _| day.as_str() >= horizon.as_str());
}

/// Check an Admin key before saving it: one small cost-report read.
pub async fn verify_admin_key(admin_key: &str) -> Result<(), String> {
    let today = Utc::now().date_naive();
    fetch_cost_report(admin_key, today, today + chrono::Duration::days(1))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountKind, BillingLimits};
    use serde_json::json;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn cost_page_is_read_in_dollars_per_day_and_model() {
        let body = json!({
            "data": [
                {"starting_at": "2026-10-01T00:00:00Z", "ending_at": "2026-10-02T00:00:00Z",
                 "results": [
                    {"amount": "1234.5", "currency": "USD", "model": "claude-opus-5-5"},
                    {"amount": "100", "currency": "USD", "model": null, "cost_type": "web_search"}
                 ]},
                {"starting_at": "2026-10-02T00:00:00Z", "ending_at": "2026-10-03T00:00:00Z",
                 "results": []}
            ],
            "has_more": true,
            "next_page": "page_2"
        });
        let (rows, next) = parse_cost_page(&body).unwrap();
        assert_eq!(next.as_deref(), Some("page_2"));
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            ("2026-10-01".into(), "claude-opus-5-5".into(), 12.345)
        );
        assert_eq!(rows[1].1, "web_search");
        assert!((rows[1].2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn last_page_has_no_cursor() {
        let body = json!({"data": [], "has_more": false, "next_page": null});
        assert_eq!(parse_cost_page(&body).unwrap().1, None);
    }

    #[test]
    fn usage_is_priced_with_cache_write_tiers_and_fast_mode() {
        let usage = json!({
            "input_tokens": 1_000_000,
            "output_tokens": 1_000_000,
            "cache_read_input_tokens": 1_000_000,
            "cache_creation_input_tokens": 2_000_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1_000_000, "ephemeral_1h_input_tokens": 1_000_000}
        });
        // Opus 5.5: 4 + 20 + 0.20 + 4*1.25 + 4*2.
        let usd = price_usage("claude-opus-5-5", &usage).unwrap();
        assert!((usd - 37.2).abs() < 1e-9, "{usd}");
        let mut fast = usage.clone();
        fast["speed"] = json!("fast");
        assert!((price_usage("claude-opus-5-5", &fast).unwrap() - 74.4).abs() < 1e-9);
        // The more specific prefix wins: Fable 5.1 reads are cheaper than Fable 5's.
        assert_eq!(price_per_mtok("claude-fable-5-1").unwrap().2, 0.25);
        assert_eq!(price_per_mtok("claude-fable-5").unwrap().2, 1.0);
        assert!(price_usage("gpt-5", &usage).is_none());
    }

    #[test]
    fn estimate_counts_each_request_once_and_only_while_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let line = |ts: &str, req: &str| {
            json!({
                "type": "assistant", "timestamp": ts, "requestId": req,
                "message": {"model": "claude-sonnet-5-5", "usage": {"input_tokens": 1_000_000, "output_tokens": 0}}
            })
            .to_string()
        };
        let text = [
            line("2026-10-10T10:00:00Z", "req_a"),
            // Same request, second content block.
            line("2026-10-10T10:00:01Z", "req_a"),
            line("2026-10-10T10:30:00Z", "req_b"),
            // Outside the period.
            line("2026-10-10T12:00:00Z", "req_c"),
            r#"{"type":"user","timestamp":"2026-10-10T10:10:00Z"}"#.to_string(),
        ]
        .join("\n");
        std::fs::write(project.join("s.jsonl"), text).unwrap();
        let spans = [Span {
            from: at("2026-10-10T09:55:00Z"),
            to: at("2026-10-10T10:30:00Z"),
        }];
        let (daily, by_model) = estimate(&mut TranscriptIndex::default(), dir.path(), &spans);
        assert!((daily["2026-10-10"] - 4.0).abs() < 1e-9);
        assert!(
            (by_model[&("2026-10".to_string(), "claude-sonnet-5-5".to_string())] - 4.0).abs()
                < 1e-9
        );
    }

    #[test]
    fn estimate_reads_only_what_was_appended() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.jsonl");
        let line = |ts: &str, req: &str| {
            json!({
                "type": "assistant", "timestamp": ts, "requestId": req,
                "message": {"model": "claude-sonnet-5-5", "usage": {"input_tokens": 1_000_000, "output_tokens": 0}}
            })
            .to_string()
        };
        let spans = [Span {
            from: at("2026-10-10T09:00:00Z"),
            to: at("2026-10-10T11:00:00Z"),
        }];
        let mut index = TranscriptIndex::default();
        let total = |index: &mut TranscriptIndex| {
            estimate(index, dir.path(), &spans).0.values().sum::<f64>()
        };

        // The second line is still being written: no newline yet.
        let first = line("2026-10-10T10:00:00Z", "req_a");
        let second = line("2026-10-10T10:05:00Z", "req_b");
        std::fs::write(&file, format!("{first}\n{}", &second[..20])).unwrap();
        assert!((total(&mut index) - 2.0).abs() < 1e-9);

        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        // The rest of that line, then a second block of a request already counted.
        write!(
            handle,
            "{}\n{}\n",
            &second[20..],
            line("2026-10-10T10:05:01Z", "req_b")
        )
        .unwrap();
        drop(handle);
        assert!((total(&mut index) - 4.0).abs() < 1e-9);
        assert_eq!(index.replies.len(), 2);
        // Nothing new: nothing changes.
        assert!((total(&mut index) - 4.0).abs() < 1e-9);
    }

    fn spans(cache: &BillingCache, key: &str) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        cache.rows[key]
            .active_spans
            .iter()
            .map(|span| (span.from, span.to))
            .collect()
    }

    #[test]
    fn an_account_seen_live_twice_was_live_in_between() {
        let mut cache = BillingCache::default();
        note_live(&mut cache, Some(("api", true)), at("2026-10-10T10:00:00Z"));
        // cc-logins was closed overnight; Claude Code kept using the key.
        note_live(&mut cache, Some(("api", true)), at("2026-10-11T09:00:00Z"));
        assert_eq!(
            spans(&cache, "api"),
            [(at("2026-10-10T10:00:00Z"), at("2026-10-11T09:00:00Z"))]
        );
    }

    #[test]
    fn a_switch_ends_the_period_when_it_happens() {
        let mut cache = BillingCache::default();
        note_live(&mut cache, Some(("api", true)), at("2026-10-10T10:00:00Z"));
        note_live(&mut cache, Some(("api", true)), at("2026-10-10T10:04:00Z"));
        // Switched to a subscription; the switch publishes a snapshot at once.
        note_live(&mut cache, Some(("sub", false)), at("2026-10-10T10:06:00Z"));
        note_live(&mut cache, Some(("sub", false)), at("2026-10-10T12:00:00Z"));
        note_live(&mut cache, Some(("api", true)), at("2026-10-10T13:00:00Z"));
        assert_eq!(
            spans(&cache, "api"),
            [
                (at("2026-10-10T10:00:00Z"), at("2026-10-10T10:06:00Z")),
                (at("2026-10-10T13:00:00Z"), at("2026-10-10T13:00:00Z")),
            ]
        );
        assert!(!cache.rows.contains_key("sub"));
    }

    #[test]
    fn a_change_found_after_a_long_gap_is_not_counted_as_use() {
        let mut cache = BillingCache::default();
        note_live(&mut cache, Some(("api", true)), at("2026-10-10T10:00:00Z"));
        // Next launch, a day later, finds another account live.
        note_live(&mut cache, Some(("sub", false)), at("2026-10-11T10:00:00Z"));
        assert_eq!(
            spans(&cache, "api"),
            [(at("2026-10-10T10:00:00Z"), at("2026-10-10T10:00:00Z"))]
        );
    }

    #[test]
    fn a_request_counts_at_its_final_size() {
        let dir = tempfile::tempdir().unwrap();
        let line = |out: u64| {
            json!({
                "type": "assistant", "timestamp": "2026-10-10T10:00:00Z", "requestId": "req_a",
                "message": {"model": "claude-sonnet-5-5", "usage": {"input_tokens": 0, "output_tokens": out}}
            })
            .to_string()
        };
        // One line per content block, output growing as the reply streams.
        let text = [line(100_000), line(500_000), line(1_000_000)].join("\n") + "\n";
        std::fs::write(dir.path().join("s.jsonl"), text).unwrap();
        let spans = [Span {
            from: at("2026-10-10T09:00:00Z"),
            to: at("2026-10-10T11:00:00Z"),
        }];
        let (daily, _) = estimate(&mut TranscriptIndex::default(), dir.path(), &spans);
        // 1M output tokens at $10/MTok.
        assert!((daily["2026-10-10"] - 10.0).abs() < 1e-9);
    }

    #[test]
    fn view_totals_the_month_and_the_balance_left() {
        let mut row = Row::default();
        for (day, usd) in [
            ("2026-09-30", 5.0),
            ("2026-10-01", 2.0),
            ("2026-10-09", 3.0),
            ("2026-10-10", 1.5),
        ] {
            row.daily.insert(day.into(), usd);
        }
        let account = Account {
            kind: AccountKind::ApiKey,
            billing_limits: BillingLimits {
                monthly_limit_usd: Some(20.0),
                prepaid_balance_usd: Some(200.0),
                balance_set_at: Some("2026-10-09T08:00:00Z".into()),
            },
            ..Account::default()
        };
        let billing = view(&row, &account, false, at("2026-10-10T12:00:00Z"));
        assert!((billing.month_to_date_usd - 6.5).abs() < 1e-9);
        assert!((billing.today_usd - 1.5).abs() < 1e-9);
        // Oct 4 to 10: the 1st is outside the week.
        assert!((billing.last7d_usd - 4.5).abs() < 1e-9);
        assert!((billing.balance_left_usd.unwrap() - 195.5).abs() < 1e-9);
        assert_eq!(billing.daily.len(), 31);
        assert_eq!(billing.daily.last().unwrap().day, "2026-10-10");
        assert_eq!(billing.resets_at, "2026-11-01T00:00:00+00:00");
        assert!(!billing.has_admin_key);
    }

    #[test]
    fn december_resets_into_the_next_year() {
        assert_eq!(
            next_month_start(at("2026-12-15T00:00:00Z")).to_rfc3339(),
            "2027-01-01T00:00:00+00:00"
        );
    }
}
