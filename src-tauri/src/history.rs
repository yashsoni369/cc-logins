//! Local SQLite store for account usage history.
//!
//! Every competing account switcher shows only the current instant of quota
//! usage. The thing that makes this app worth having open is that it
//! remembers: it can show *burn rate* — how fast an account is climbing
//! toward its limit — across days and weeks, not just the current reading.
//! That requires persisting every distinct usage measurement locally. This
//! module is that persistence layer. It is entirely local: no server, no
//! sync, no upload — a single SQLite file under the app's data directory.
//!
//! # Shared types
//!
//! `Snapshot` / `Account` / `Usage` / `UsageWindow` / `Environment` are no
//! longer duplicated here — they are imported from
//! [`crate::model`], which is the single source of truth for the shapes that
//! cross the Tauri IPC boundary. This module previously carried its own
//! byte-for-byte copies (same camelCase field names, same flat `UsageWindow`
//! shape); that duplication has been removed.
//!
//! Identity for history rows used to be a second, locally-duplicated copy of
//! [`crate::model::Account::stable_key`] (a `stable_account_key` function
//! that used to live in this module), deliberately kept separate because the
//! two hashed slightly different
//! inputs — this module trimmed the email before hashing, `Account::stable_key`
//! did not — and reconciling them silently would have risked splitting or
//! merging a real user's history series. `model.rs` was subsequently updated
//! to trim as well, and a byte-for-byte comparison (padded/mixed-case email,
//! and an account with an `organizationUuid`) confirmed the two now agree in
//! every case, so the duplicate has been deleted: this module calls
//! [`crate::model::Account::stable_key`] directly.
//!
//! # Schema
//!
//! Three tables (`PRAGMA user_version` gates migrations — see [`migrate`]):
//!
//! - `samples` — one row per *distinct* measurement: `account_key`,
//!   ISO-8601 `timestamp`, `five_hour_pct`, `seven_day_pct`, and
//!   `binding_pct` (the worst of every window that gates the account,
//!   including scoped ones — the same number `bindingUtilisation()` in
//!   `types.ts` computes). Indexed on `(account_key, timestamp)` since every
//!   read below is "this account, this time range".
//! - `scoped_samples` — per-model windows (`{ name: "Fable", pct: 25 }` in
//!   the fixture), one row per `(account_key, timestamp, model_name)`. See
//!   the comment on [`migrate`] for why this is a normalized table and not a
//!   JSON column.
//! - `daily_rollups` — pre-aggregated `(account_key, day)` min/max/avg/count,
//!   populated by [`HistoryStore::prune`] as raw rows age out. See that
//!   method's doc comment for the retention reasoning.
//!
//! # Identity
//!
//! Accounts are keyed by [`crate::model::Account::stable_key`], never by
//! email or slot number — both are mutable (slots are user-reorderable,
//! emails get renamed) and either would fork or merge a time series across a
//! rename.
//!
//! # Idempotent writes
//!
//! [`HistoryStore::record`] deduplicates on `(account_key, timestamp)`. See
//! its doc comment for why that matters: the poller runs every 60s but the
//! usage API refreshes far less often, so the *same* measurement arrives
//! dozens of times before the underlying number moves.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, NaiveDate, SecondsFormat, Utc, Weekday};
use rusqlite::{params, Connection};
use serde::Serialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Input shapes — what `record()` accepts. These are `crate::model`'s shared
// `Snapshot` / `Account` / `Usage` types, the same shapes that cross the
// Tauri IPC boundary. Account identity is `crate::model::Account::stable_key`
// — see the module doc comment for why this module no longer keeps its own
// copy of that function.
// ---------------------------------------------------------------------------

use crate::model::{Snapshot, Usage};

/// The highest utilisation across every window that gates an account —
/// five-hour, seven-day, and every scoped per-model window. Mirrors
/// `bindingUtilisation()` in `src/types.ts`: this is the number that
/// actually decides when the account gets skipped, not any single window.
fn binding_pct(usage: &Usage) -> f64 {
    let mut best = 0.0_f64;
    if let Some(w) = &usage.five_hour {
        best = best.max(w.pct);
    }
    if let Some(w) = &usage.seven_day {
        best = best.max(w.pct);
    }
    for w in usage.scoped.iter().flatten() {
        best = best.max(w.pct);
    }
    best
}

// ---------------------------------------------------------------------------
// Output shapes — everything here derives `Serialize` with camelCase so it
// crosses the Tauri IPC boundary matching `src/types.ts` conventions, even
// though none of these particular shapes exist in `types.ts` yet (history is
// a new screen).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedSample {
    pub name: String,
    pub pct: f64,
}

/// One recorded measurement, as returned by [`HistoryStore::series`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub account_key: String,
    pub timestamp: String,
    pub five_hour_pct: Option<f64>,
    pub seven_day_pct: Option<f64>,
    pub binding_pct: f64,
    pub scoped: Vec<ScopedSample>,
}

/// Min/max/avg/count of `binding_pct` for one calendar day (UTC), as
/// returned by [`HistoryStore::daily_rollup`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayStat {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub min_pct: f64,
    pub max_pct: f64,
    pub avg_pct: f64,
    pub sample_count: i64,
}

/// Backs the stat row on the History screen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistorySummary {
    /// Average `binding_pct` over the trailing 7 days (always 7, regardless
    /// of the `days` window requested — this is specifically the *weekly*
    /// figure the name promises, not `days`-scaled).
    pub weekly_average_pct: f64,
    /// Count of measurements, across every account, where `binding_pct`
    /// reached 100 within the requested `days` window. For days old enough
    /// to have been compacted by [`HistoryStore::prune`] into a daily
    /// rollup, this counts *days* whose `max_pct` hit 100 rather than
    /// individual measurements (the per-sample count for that day no longer
    /// exists) — documented undercount, not a bug; see `prune`'s doc
    /// comment.
    pub times_at_100_pct: i64,
    /// The weekday (UTC, full English name) with the highest *average*
    /// `binding_pct` within the requested `days` window — i.e. "which day
    /// do accounts tend to run hottest", not "which day had the most
    /// samples" (sample count mostly reflects polling frequency, not real
    /// usage). `None` when there is no data in the window.
    pub busiest_weekday: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("failed to create history data directory {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to open history database at {path}: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("history database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, HistoryError>;

// ---------------------------------------------------------------------------
// Schema / migrations
// ---------------------------------------------------------------------------

const CURRENT_SCHEMA_VERSION: i32 = 1;

/// Default raw-sample retention for [`HistoryStore::prune`], in days.
///
/// Raw samples land only when the measurement actually changes (dedup keeps
/// a flat reading from writing hundreds of rows — see `record`'s doc
/// comment), so real row growth is closer to "a few per hour" than "one per
/// poll", but it's still unbounded over months of uptime across several
/// accounts. Two weeks of raw granularity is enough to show *this week vs.
/// last week* burn-rate detail, which is the case someone actually opens the
/// history screen to look at; anything older is a shape question ("was I
/// consistently near the cap in June?") that a daily min/max/avg answers
/// just as well at a fraction of the storage — hence downsampling instead of
/// deleting outright. See `prune`'s doc comment for the downsample mechanics.
pub const DEFAULT_RAW_RETENTION_DAYS: i64 = 14;

/// Create the schema if this is a fresh database, or step it forward if
/// `user_version` is behind [`CURRENT_SCHEMA_VERSION`]. `user_version` is a
/// SQLite pragma stored in the database file itself (not a table), so it
/// survives even a schema-less empty file and needs no bootstrap query.
///
/// # Why `scoped_samples` is a separate table and not a JSON column
///
/// Per-model windows (the `scoped` array — `{ name: "Fable", pct: 25 }` in
/// the fixture) need to support the exact same query shape as the main
/// samples: "this account, this model, this time range", so it can be
/// charted per-model the same way the binding percentage is. Two options
/// were on the table:
///
/// - A `scoped_json TEXT` column on `samples` holding the serialized array.
/// - A normalized `scoped_samples` table.
///
/// JSON-in-a-column loses the index. Every read in this module is time-
/// ranged (`series`, `daily_rollup`, `summary`), and SQLite's JSON1
/// functions can't use a B-tree index on a value packed inside a TEXT blob —
/// filtering by model name and time range would mean deserializing and
/// scanning every row's JSON in the range rather than a `WHERE model_name =
/// ? AND timestamp BETWEEN ? AND ?` index seek. It also makes the rollup
/// math (`MIN`/`MAX`/`AVG`/`COUNT` per model per day) require `json_each` in
/// every aggregate query instead of plain `GROUP BY`. A normalized table
/// with its own `(account_key, model_name, timestamp)` index keeps every
/// query in this module plain SQL with index support, at the cost of one
/// extra table — for the handful of scoped windows an account actually has
/// (the fixture shows one — "Fable" — per account), that cost is trivial.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version < 1 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS samples (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key  TEXT NOT NULL,
                timestamp    TEXT NOT NULL,
                five_hour_pct REAL,
                seven_day_pct REAL,
                binding_pct  REAL NOT NULL,
                UNIQUE(account_key, timestamp)
            );
            CREATE INDEX IF NOT EXISTS idx_samples_account_ts
                ON samples(account_key, timestamp);

            CREATE TABLE IF NOT EXISTS scoped_samples (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key  TEXT NOT NULL,
                timestamp    TEXT NOT NULL,
                model_name   TEXT NOT NULL,
                pct          REAL NOT NULL,
                UNIQUE(account_key, timestamp, model_name)
            );
            CREATE INDEX IF NOT EXISTS idx_scoped_account_model_ts
                ON scoped_samples(account_key, model_name, timestamp);

            CREATE TABLE IF NOT EXISTS daily_rollups (
                account_key  TEXT NOT NULL,
                day          TEXT NOT NULL,
                min_pct      REAL NOT NULL,
                max_pct      REAL NOT NULL,
                avg_pct      REAL NOT NULL,
                sample_count INTEGER NOT NULL,
                PRIMARY KEY (account_key, day)
            );
            ",
        )?;
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    }

    // Future migrations: `if version < 2 { ... }`, each step bringing any
    // pre-existing database forward one version at a time, finishing with a
    // `pragma_update` to the new version number.

    Ok(())
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// A local SQLite-backed store for account usage history.
///
/// Takes its directory as a parameter (rather than resolving the app data
/// dir itself) so callers — and tests — can point it at anything, including
/// a `tempfile::tempdir()`.
pub struct HistoryStore {
    conn: Mutex<Connection>,
}

impl HistoryStore {
    /// Open (creating if needed) `history.sqlite3` under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).map_err(|source| HistoryError::Io {
            path: data_dir.to_path_buf(),
            source,
        })?;

        let db_path = data_dir.join("history.sqlite3");
        let conn = Connection::open(&db_path).map_err(|source| HistoryError::Open {
            path: db_path.clone(),
            source,
        })?;

        // WAL rather than the default rollback journal: the poller (writer)
        // and the History screen (reader) run concurrently from the same
        // process, and WAL lets readers proceed without waiting on a writer
        // holding the connection mid-transaction.
        conn.pragma_update(None, "journal_mode", "WAL")?;

        migrate(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record every measured account in `snapshot`.
    ///
    /// Returns the number of *new* samples written (0 when every account's
    /// measurement was already known).
    ///
    /// # Idempotency
    ///
    /// The background poller runs on a fixed ~60s tick, but the upstream
    /// usage API only refreshes on its own schedule — every few minutes, not
    /// every poll. That means the overwhelming majority of poll ticks
    /// observe the *exact same* `usageFetchedAt` for a given account as the
    /// previous tick: nothing new happened upstream. Recording every tick
    /// unconditionally would insert dozens of byte-identical rows per real
    /// measurement, and a chart drawn from that data would be a flat line
    /// made of hundreds of overlapping points — technically correct,
    /// practically useless, and a steadily growing database for no signal
    /// gained.
    ///
    /// So this dedups on the measurement timestamp rather than on wall-clock
    /// insert time: `(account_key, timestamp)` is `UNIQUE` on `samples` (and
    /// `(account_key, timestamp, model_name)` on `scoped_samples`), and the
    /// insert uses `INSERT OR IGNORE`. Re-recording the same
    /// `usageFetchedAt` for the same account is a guaranteed no-op, however
    /// many times the poller happens to observe it.
    ///
    /// An account with `usage: None` or `usageFetchedAt: None` is skipped —
    /// there is nothing to key a row on. An account whose
    /// `usageFetchedAt` fails to parse as RFC 3339 is also skipped (logged
    /// via `log::warn!`) rather than failing the whole batch: one
    /// misbehaving account should not stop every other account in the same
    /// snapshot from being recorded.
    pub fn record(&self, snapshot: &Snapshot) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        let mut inserted = 0usize;

        for env in &snapshot.environments {
            for account in &env.accounts {
                let Some(usage) = &account.usage else {
                    continue;
                };
                let Some(raw_timestamp) = &account.usage_fetched_at else {
                    continue;
                };

                let timestamp = match DateTime::parse_from_rfc3339(raw_timestamp) {
                    Ok(dt) => dt
                        .with_timezone(&Utc)
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                    Err(err) => {
                        log::warn!(
                            "history: skipping {} — unparseable usageFetchedAt {raw_timestamp:?}: {err}",
                            account.email
                        );
                        continue;
                    }
                };

                let account_key = account.stable_key();
                let five_hour_pct = usage.five_hour.as_ref().map(|w| w.pct);
                let seven_day_pct = usage.seven_day.as_ref().map(|w| w.pct);
                let binding = binding_pct(usage);

                let changes = tx.execute(
                    "INSERT OR IGNORE INTO samples \
                     (account_key, timestamp, five_hour_pct, seven_day_pct, binding_pct) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        account_key,
                        timestamp,
                        five_hour_pct,
                        seven_day_pct,
                        binding
                    ],
                )?;

                if changes > 0 {
                    inserted += 1;
                    for window in usage.scoped.iter().flatten() {
                        let Some(name) = &window.name else { continue };
                        tx.execute(
                            "INSERT OR IGNORE INTO scoped_samples \
                             (account_key, timestamp, model_name, pct) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![account_key, timestamp, name, window.pct],
                        )?;
                    }
                }
            }
        }

        tx.commit()?;
        Ok(inserted)
    }

    /// Every sample for `account_key` with `since <= timestamp <= until`,
    /// ascending, each with its scoped per-model windows attached.
    pub fn series(
        &self,
        account_key: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<Sample>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let since_ts = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let until_ts = until.to_rfc3339_opts(SecondsFormat::Secs, true);

        let mut samples: Vec<Sample> = {
            let mut stmt = conn.prepare(
                "SELECT timestamp, five_hour_pct, seven_day_pct, binding_pct \
                 FROM samples \
                 WHERE account_key = ?1 AND timestamp >= ?2 AND timestamp <= ?3 \
                 ORDER BY timestamp ASC",
            )?;
            let rows = stmt.query_map(params![account_key, since_ts, until_ts], |row| {
                Ok(Sample {
                    account_key: account_key.to_string(),
                    timestamp: row.get(0)?,
                    five_hour_pct: row.get(1)?,
                    seven_day_pct: row.get(2)?,
                    binding_pct: row.get(3)?,
                    scoped: Vec::new(),
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut scoped_by_ts: HashMap<String, Vec<ScopedSample>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT timestamp, model_name, pct \
                 FROM scoped_samples \
                 WHERE account_key = ?1 AND timestamp >= ?2 AND timestamp <= ?3 \
                 ORDER BY timestamp ASC",
            )?;
            let rows = stmt.query_map(params![account_key, since_ts, until_ts], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ScopedSample {
                        name: row.get(1)?,
                        pct: row.get(2)?,
                    },
                ))
            })?;
            for row in rows {
                let (ts, scoped) = row?;
                scoped_by_ts.entry(ts).or_default().push(scoped);
            }
        }

        for sample in &mut samples {
            if let Some(scoped) = scoped_by_ts.remove(&sample.timestamp) {
                sample.scoped = scoped;
            }
        }

        Ok(samples)
    }

    /// Per-day min/max/avg/count of `binding_pct` for `account_key` over the
    /// trailing `days` days. Backs the 30d/90d chart ranges.
    ///
    /// Transparently combines raw `samples` (for days not yet pruned) with
    /// `daily_rollups` (for days [`prune`](Self::prune) has already
    /// compacted) — a day that still has raw rows always wins over any
    /// same-day rollup entry, since a day only gets a rollup once its raw
    /// rows have been deleted (see `prune`'s doc comment), so in steady
    /// state the two never actually disagree for the same day.
    pub fn daily_rollup(&self, account_key: &str, days: i64) -> Result<Vec<DayStat>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let since = Utc::now() - Duration::days(days);
        let since_ts = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let since_day = since.format("%Y-%m-%d").to_string();

        let mut by_day: BTreeMap<String, DayStat> = BTreeMap::new();

        {
            let mut stmt = conn.prepare(
                "SELECT day, min_pct, max_pct, avg_pct, sample_count \
                 FROM daily_rollups \
                 WHERE account_key = ?1 AND day >= ?2",
            )?;
            let rows = stmt.query_map(params![account_key, since_day], |row| {
                Ok(DayStat {
                    day: row.get(0)?,
                    min_pct: row.get(1)?,
                    max_pct: row.get(2)?,
                    avg_pct: row.get(3)?,
                    sample_count: row.get(4)?,
                })
            })?;
            for row in rows {
                let stat = row?;
                by_day.insert(stat.day.clone(), stat);
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT substr(timestamp, 1, 10) AS day, \
                        MIN(binding_pct), MAX(binding_pct), AVG(binding_pct), COUNT(*) \
                 FROM samples \
                 WHERE account_key = ?1 AND timestamp >= ?2 \
                 GROUP BY day",
            )?;
            let rows = stmt.query_map(params![account_key, since_ts], |row| {
                Ok(DayStat {
                    day: row.get(0)?,
                    min_pct: row.get(1)?,
                    max_pct: row.get(2)?,
                    avg_pct: row.get(3)?,
                    sample_count: row.get(4)?,
                })
            })?;
            for row in rows {
                let stat = row?;
                by_day.insert(stat.day.clone(), stat);
            }
        }

        Ok(by_day.into_values().collect())
    }

    /// Stats for the History screen's stat row, over the trailing `days`
    /// days (see [`HistorySummary`] field docs for exactly what each number
    /// means and how rollup-compacted days are handled).
    pub fn summary(&self, days: i64) -> Result<HistorySummary> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let now = Utc::now();
        let window_since = now - Duration::days(days);
        let week_since = now - Duration::days(7);

        Ok(HistorySummary {
            weekly_average_pct: weighted_avg_since(&conn, week_since)?,
            times_at_100_pct: count_at_100_since(&conn, window_since)?,
            busiest_weekday: busiest_weekday_since(&conn, window_since)?,
        })
    }

    /// Delete raw samples older than `keep_days`, after folding each about-
    /// to-be-deleted `(account_key, day)` group into `daily_rollups`.
    ///
    /// Returns the number of raw `samples` rows deleted.
    ///
    /// # Why downsample instead of just deleting
    ///
    /// A raw sample only lands when the measurement actually changes (see
    /// `record`'s idempotency doc comment), so growth is bounded but not
    /// small: several accounts, each picking up a handful of distinct
    /// readings per hour, for as long as the app has been installed. Once a
    /// day is old enough that nobody is going to zoom into its hour-by-hour
    /// detail anymore, the only thing still worth keeping is its shape — the
    /// min/max/avg for that day — which is exactly what `daily_rollups`
    /// stores, at one row per account per day forever instead of dozens.
    /// That is what keeps the 90-day chart range in `daily_rollup` cheap
    /// indefinitely instead of scanning a growing raw table.
    ///
    /// The rollup is computed and upserted *before* the delete, in the same
    /// transaction, so a crash mid-`prune` can never lose a day's data
    /// entirely (`ON CONFLICT` merges rather than overwrites, so re-running
    /// `prune` after a partial run — or simply calling it more than once a
    /// day as the cutoff advances into a day that still had some raw rows
    /// left — accumulates correctly instead of double counting or
    /// clobbering an existing rollup).
    ///
    /// [`DEFAULT_RAW_RETENTION_DAYS`] is the suggested `keep_days` for a
    /// scheduled caller; this method takes it as a parameter rather than
    /// hardcoding it so callers (and tests) can choose their own.
    pub fn prune(&self, keep_days: i64) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;

        let cutoff = Utc::now() - Duration::days(keep_days);
        let cutoff_ts = cutoff.to_rfc3339_opts(SecondsFormat::Secs, true);

        let expiring: Vec<(String, String, f64, f64, f64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT account_key, substr(timestamp, 1, 10) AS day, \
                        MIN(binding_pct), MAX(binding_pct), AVG(binding_pct), COUNT(*) \
                 FROM samples \
                 WHERE timestamp < ?1 \
                 GROUP BY account_key, day",
            )?;
            // Bind the collected rows before the block ends: the query_map
            // iterator borrows `stmt`, so it cannot be the tail expression —
            // `stmt` would drop while still borrowed.
            let rows = stmt
                .query_map(params![cutoff_ts], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };

        for (account_key, day, min_pct, max_pct, avg_pct, count) in expiring {
            tx.execute(
                "INSERT INTO daily_rollups (account_key, day, min_pct, max_pct, avg_pct, sample_count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(account_key, day) DO UPDATE SET \
                    min_pct = MIN(daily_rollups.min_pct, excluded.min_pct), \
                    max_pct = MAX(daily_rollups.max_pct, excluded.max_pct), \
                    avg_pct = (daily_rollups.avg_pct * daily_rollups.sample_count \
                               + excluded.avg_pct * excluded.sample_count) \
                              / (daily_rollups.sample_count + excluded.sample_count), \
                    sample_count = daily_rollups.sample_count + excluded.sample_count",
                params![account_key, day, min_pct, max_pct, avg_pct, count],
            )?;
        }

        let deleted = tx.execute(
            "DELETE FROM samples WHERE timestamp < ?1",
            params![cutoff_ts],
        )?;
        tx.execute(
            "DELETE FROM scoped_samples WHERE timestamp < ?1",
            params![cutoff_ts],
        )?;

        tx.commit()?;
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------
// summary() helpers
// ---------------------------------------------------------------------------

/// Sample-count-weighted average `binding_pct` across every account since
/// `since`, combining raw `samples` and `daily_rollups` (see `daily_rollup`
/// for why the two don't double count in steady state). `0.0` when there is
/// no data at all in range.
fn weighted_avg_since(conn: &Connection, since: DateTime<Utc>) -> Result<f64> {
    let since_ts = since.to_rfc3339_opts(SecondsFormat::Secs, true);
    let since_day = since.format("%Y-%m-%d").to_string();

    let (raw_sum, raw_count): (f64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(binding_pct), 0), COUNT(*) FROM samples WHERE timestamp >= ?1",
        params![since_ts],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (roll_sum, roll_count): (f64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(avg_pct * sample_count), 0), COALESCE(SUM(sample_count), 0) \
         FROM daily_rollups WHERE day >= ?1",
        params![since_day],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let total_count = raw_count + roll_count;
    if total_count == 0 {
        return Ok(0.0);
    }
    Ok((raw_sum + roll_sum) / total_count as f64)
}

/// Count of times any account's `binding_pct` reached 100 since `since`. See
/// [`HistorySummary::times_at_100_pct`] for the documented undercount on
/// rollup-compacted days.
fn count_at_100_since(conn: &Connection, since: DateTime<Utc>) -> Result<i64> {
    let since_ts = since.to_rfc3339_opts(SecondsFormat::Secs, true);
    let since_day = since.format("%Y-%m-%d").to_string();

    let raw: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE timestamp >= ?1 AND binding_pct >= 100.0",
        params![since_ts],
        |row| row.get(0),
    )?;
    let rolled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM daily_rollups WHERE day >= ?1 AND max_pct >= 100.0",
        params![since_day],
        |row| row.get(0),
    )?;
    Ok(raw + rolled)
}

/// The weekday (UTC) with the highest average `binding_pct` since `since`,
/// across raw samples and daily rollups (rollup days are weighted by their
/// `sample_count` so a day that was itself an average of many readings
/// doesn't count the same as a single raw reading).
fn busiest_weekday_since(conn: &Connection, since: DateTime<Utc>) -> Result<Option<String>> {
    let since_ts = since.to_rfc3339_opts(SecondsFormat::Secs, true);
    let since_day = since.format("%Y-%m-%d").to_string();

    let mut sum: HashMap<Weekday, f64> = HashMap::new();
    let mut count: HashMap<Weekday, i64> = HashMap::new();

    {
        let mut stmt =
            conn.prepare("SELECT timestamp, binding_pct FROM samples WHERE timestamp >= ?1")?;
        let rows = stmt.query_map(params![since_ts], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (ts, pct) = row?;
            if let Ok(dt) = DateTime::parse_from_rfc3339(&ts) {
                let wd = dt.with_timezone(&Utc).weekday();
                *sum.entry(wd).or_insert(0.0) += pct;
                *count.entry(wd).or_insert(0) += 1;
            }
        }
    }
    {
        let mut stmt =
            conn.prepare("SELECT day, avg_pct, sample_count FROM daily_rollups WHERE day >= ?1")?;
        let rows = stmt.query_map(params![since_day], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (day, avg_pct, day_count) = row?;
            if let Ok(date) = NaiveDate::parse_from_str(&day, "%Y-%m-%d") {
                let wd = date.weekday();
                *sum.entry(wd).or_insert(0.0) += avg_pct * day_count as f64;
                *count.entry(wd).or_insert(0) += day_count;
            }
        }
    }

    Ok(count
        .into_iter()
        .filter(|&(_, c)| c > 0)
        .map(|(wd, c)| (wd, sum[&wd] / c as f64))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(wd, _)| weekday_name(wd)))
}

fn weekday_name(wd: Weekday) -> String {
    match wd {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, EnvKind, EnvStatus, Environment, UsageStatus, UsageWindow};

    /// Build a one-environment, one-account snapshot with a given identity,
    /// timestamp, and pcts — enough for every test below without dragging in
    /// the full fixture (whose top-level shape, a bare `accounts` array with
    /// no `environments` wrapper, doesn't actually match `types.ts`'s
    /// `Snapshot` — that mismatch belongs to `model.rs`'s test, not this
    /// module).
    fn snapshot_for(
        org_uuid: Option<&str>,
        email: &str,
        timestamp: &str,
        five_hour_pct: f64,
        seven_day_pct: f64,
        scoped: &[(&str, f64)],
    ) -> Snapshot {
        Snapshot {
            schema_version: 1,
            active_account_number: Some(1),
            environments: vec![Environment {
                id: "native".into(),
                label: "Native".into(),
                path: String::new(),
                kind: EnvKind::Native,
                status: EnvStatus::Live,
                last_seen_seconds: None,
                has_credentials: None,
                accounts: vec![Account {
                    number: 1,
                    email: email.to_string(),
                    organization_uuid: org_uuid.map(str::to_string),
                    is_organization: Some(org_uuid.is_some()),
                    active: false,
                    usage_status: UsageStatus::Ok,
                    usage: Some(Usage {
                        five_hour: Some(UsageWindow {
                            pct: five_hour_pct,
                            ..Default::default()
                        }),
                        seven_day: Some(UsageWindow {
                            pct: seven_day_pct,
                            expected_pct: Some(42.0),
                            ahead_of_pace: Some(false),
                            will_last_to_reset: Some(true),
                            ..Default::default()
                        }),
                        scoped: Some(
                            scoped
                                .iter()
                                .map(|(name, pct)| UsageWindow {
                                    pct: *pct,
                                    name: Some(name.to_string()),
                                    ..Default::default()
                                })
                                .collect(),
                        ),
                    }),
                    usage_fetched_at: Some(timestamp.to_string()),
                    usage_age_seconds: Some(0.0),
                    ..Default::default()
                }],
            }],
        }
    }

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(dir.path()).unwrap();
        (dir, store)
    }

    // -- schema creation ---------------------------------------------------

    #[test]
    fn open_creates_schema_and_sets_user_version() {
        let (_dir, store) = store();
        let conn = store.conn.lock().unwrap_or_else(|p| p.into_inner());

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        names.retain(|n| !n.starts_with("sqlite_"));
        assert_eq!(names, vec!["daily_rollups", "samples", "scoped_samples"]);
    }

    #[test]
    fn open_is_reentrant_on_an_existing_database() {
        // Opening the same directory twice (e.g. app restart) must not fail
        // or wipe existing data.
        let dir = tempfile::tempdir().unwrap();
        let first = HistoryStore::open(dir.path()).unwrap();
        let snap = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            10.0,
            20.0,
            &[],
        );
        first.record(&snap).unwrap();
        drop(first);

        let second = HistoryStore::open(dir.path()).unwrap();
        let key = snap.environments[0].accounts[0].stable_key();
        let series = second
            // Wide window: the fixtures use fixed calendar timestamps, so a
            // `now ± 1 day` range silently stops matching them as the real
            // clock moves past the fixture date.
            .series(
                &key,
                Utc::now() - Duration::days(3650),
                Utc::now() + Duration::days(1),
            )
            .unwrap();
        assert_eq!(series.len(), 1);
    }

    // -- identity ------------------------------------------------------------

    /// `history.rs` no longer has its own identity function — it calls
    /// [`crate::model::Account::stable_key`] directly (see the module doc
    /// comment for the byte-for-byte equivalence check that justified
    /// deleting the local, trimming copy). This test now exercises that call
    /// path end-to-end instead of a since-removed local function.
    #[test]
    fn stable_key_prefers_org_uuid_and_hashes_email_otherwise() {
        let with_org = snapshot_for(
            Some("org-123"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        assert_eq!(
            with_org.environments[0].accounts[0].stable_key(),
            "org:org-123"
        );

        let no_org = snapshot_for(
            None,
            "Person@Example.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        let key = no_org.environments[0].accounts[0].stable_key();
        assert!(key.starts_with("email:"));
        assert_ne!(
            key, "email:Person@Example.com",
            "must not store the raw email as the key"
        );

        // Case-insensitive: same account, different casing, same key.
        let lower = snapshot_for(
            None,
            "person@example.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        assert_eq!(key, lower.environments[0].accounts[0].stable_key());
    }

    /// Padded email must key identically to its trimmed form — both this
    /// module's writes (`record`) and any reader must agree, or a stray
    /// space around a stored address would silently fork the history series.
    #[test]
    fn stable_key_trims_padded_email() {
        let padded = snapshot_for(
            None,
            "  Person@Example.COM  ",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        let clean = snapshot_for(
            None,
            "person@example.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        assert_eq!(
            padded.environments[0].accounts[0].stable_key(),
            clean.environments[0].accounts[0].stable_key()
        );
    }

    // -- idempotent repeat measurement ---------------------------------------

    #[test]
    fn recording_the_same_measurement_repeatedly_is_a_no_op() {
        let (_dir, store) = store();
        let snap = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T12:00:00Z",
            16.0,
            81.0,
            &[("Fable", 22.0)],
        );

        let first = store.record(&snap).unwrap();
        assert_eq!(first, 1, "first sighting of a measurement is recorded");

        // The poller ticks 60 times before the usage API refreshes — same
        // snapshot, same usageFetchedAt, every time.
        for _ in 0..60 {
            let again = store.record(&snap).unwrap();
            assert_eq!(
                again, 0,
                "repeat of an already-seen timestamp must not insert a new row"
            );
        }

        let key = snap.environments[0].accounts[0].stable_key();
        let series = store
            // Wide window: the fixtures use fixed calendar timestamps, so a
            // `now ± 1 day` range silently stops matching them as the real
            // clock moves past the fixture date.
            .series(
                &key,
                Utc::now() - Duration::days(3650),
                Utc::now() + Duration::days(1),
            )
            .unwrap();
        assert_eq!(
            series.len(),
            1,
            "60 repeats of one measurement must still be exactly one row"
        );
        assert_eq!(series[0].binding_pct, 81.0);
        assert_eq!(
            series[0].scoped,
            vec![ScopedSample {
                name: "Fable".into(),
                pct: 22.0
            }]
        );

        // A genuinely new measurement (different timestamp) is a new row.
        let snap2 = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T12:05:00Z",
            18.0,
            83.0,
            &[("Fable", 23.0)],
        );
        let second = store.record(&snap2).unwrap();
        assert_eq!(second, 1);
        let series = store
            // Wide window: the fixtures use fixed calendar timestamps, so a
            // `now ± 1 day` range silently stops matching them as the real
            // clock moves past the fixture date.
            .series(
                &key,
                Utc::now() - Duration::days(3650),
                Utc::now() + Duration::days(1),
            )
            .unwrap();
        assert_eq!(series.len(), 2);
    }

    #[test]
    fn accounts_without_usage_or_timestamp_are_skipped_not_errored() {
        let (_dir, store) = store();
        let mut snap = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            1.0,
            2.0,
            &[],
        );
        snap.environments[0].accounts[0].usage = None;
        assert_eq!(store.record(&snap).unwrap(), 0);

        let mut snap2 = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            1.0,
            2.0,
            &[],
        );
        snap2.environments[0].accounts[0].usage_fetched_at = None;
        assert_eq!(store.record(&snap2).unwrap(), 0);

        let mut snap3 = snapshot_for(Some("org-1"), "a@x.com", "not-a-timestamp", 1.0, 2.0, &[]);
        snap3.environments[0].accounts[0].usage_fetched_at = Some("not-a-timestamp".into());
        assert_eq!(store.record(&snap3).unwrap(), 0);
    }

    // -- time-ranged series query --------------------------------------------

    #[test]
    fn series_returns_only_rows_within_the_requested_range_in_order() {
        let (_dir, store) = store();
        let key_holder = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        let key = key_holder.environments[0].accounts[0].stable_key();

        for (ts, five, seven) in [
            ("2026-07-01T00:00:00Z", 10.0, 20.0),
            ("2026-07-02T00:00:00Z", 12.0, 25.0),
            ("2026-07-03T00:00:00Z", 14.0, 30.0),
            ("2026-07-10T00:00:00Z", 50.0, 60.0),
        ] {
            let snap = snapshot_for(Some("org-1"), "a@x.com", ts, five, seven, &[]);
            store.record(&snap).unwrap();
        }

        let since = DateTime::parse_from_rfc3339("2026-07-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let until = DateTime::parse_from_rfc3339("2026-07-03T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        let series = store.series(&key, since, until).unwrap();

        assert_eq!(series.len(), 2);
        assert_eq!(series[0].timestamp, "2026-07-02T00:00:00Z");
        assert_eq!(series[1].timestamp, "2026-07-03T00:00:00Z");
        assert!(series.windows(2).all(|w| w[0].timestamp <= w[1].timestamp));
    }

    // -- daily rollup math ----------------------------------------------------

    #[test]
    fn daily_rollup_computes_min_max_avg_per_day_from_raw_samples() {
        let (_dir, store) = store();
        let key_holder = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        let key = key_holder.environments[0].accounts[0].stable_key();

        let today = Utc::now().format("%Y-%m-%d").to_string();
        // Three measurements on the same UTC day, binding_pct = max(five, seven)
        // = seven_day in each case here: 20, 50, 35.
        for (hh, seven) in [("01", 20.0), ("02", 50.0), ("03", 35.0)] {
            let ts = format!("{today}T{hh}:00:00Z");
            let snap = snapshot_for(Some("org-1"), "a@x.com", &ts, 5.0, seven, &[]);
            store.record(&snap).unwrap();
        }

        let rollup = store.daily_rollup(&key, 7).unwrap();
        assert_eq!(rollup.len(), 1);
        let day = &rollup[0];
        assert_eq!(day.day, today);
        assert_eq!(day.sample_count, 3);
        assert_eq!(day.min_pct, 20.0);
        assert_eq!(day.max_pct, 50.0);
        assert!((day.avg_pct - (20.0 + 50.0 + 35.0) / 3.0).abs() < 1e-9);
    }

    // -- pruning ---------------------------------------------------------------

    #[test]
    fn prune_downsamples_old_raw_rows_into_daily_rollups_then_deletes_them() {
        let (_dir, store) = store();
        let key_holder = snapshot_for(
            Some("org-1"),
            "a@x.com",
            "2026-07-01T00:00:00Z",
            0.0,
            0.0,
            &[],
        );
        let key = key_holder.environments[0].accounts[0].stable_key();

        // Two old measurements (40 days ago) on the same day, one recent one.
        let old_day = Utc::now() - Duration::days(40);
        let old_day_str = old_day.format("%Y-%m-%d").to_string();
        for (hh, seven) in [("01", 30.0), ("13", 70.0)] {
            let ts = format!("{old_day_str}T{hh}:00:00Z");
            let snap = snapshot_for(Some("org-1"), "a@x.com", &ts, 5.0, seven, &[]);
            store.record(&snap).unwrap();
        }
        let recent_ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let recent_snap = snapshot_for(Some("org-1"), "a@x.com", &recent_ts, 5.0, 40.0, &[]);
        store.record(&recent_snap).unwrap();

        let deleted = store.prune(DEFAULT_RAW_RETENTION_DAYS).unwrap();
        assert_eq!(deleted, 2, "only the two 40-day-old raw rows are pruned");

        // Raw rows for the old day are gone...
        let raw_count: i64 = {
            let conn = store.conn.lock().unwrap_or_else(|p| p.into_inner());
            conn.query_row(
                "SELECT COUNT(*) FROM samples WHERE account_key = ?1 AND timestamp LIKE ?2",
                params![key, format!("{old_day_str}%")],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(raw_count, 0);

        // ...but a rollup for that day now exists with the correct math.
        let rollup = store.daily_rollup(&key, 90).unwrap();
        let old = rollup
            .iter()
            .find(|d| d.day == old_day_str)
            .expect("old day rolled up");
        assert_eq!(old.sample_count, 2);
        assert_eq!(old.min_pct, 30.0);
        assert_eq!(old.max_pct, 70.0);
        assert!((old.avg_pct - 50.0).abs() < 1e-9);

        // The recent sample is untouched and still shows up in the same
        // combined daily_rollup() read.
        let today_str = Utc::now().format("%Y-%m-%d").to_string();
        let recent = rollup
            .iter()
            .find(|d| d.day == today_str)
            .expect("recent day still raw");
        assert_eq!(recent.sample_count, 1);
        assert_eq!(recent.max_pct, 40.0);
    }

    #[test]
    fn prune_is_safe_to_call_repeatedly() {
        let (_dir, store) = store();
        let old_ts = (Utc::now() - Duration::days(40)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let snap = snapshot_for(Some("org-1"), "a@x.com", &old_ts, 5.0, 60.0, &[]);
        store.record(&snap).unwrap();

        assert_eq!(store.prune(DEFAULT_RAW_RETENTION_DAYS).unwrap(), 1);
        // Nothing left to delete the second time, and it must not error or
        // double-count the already-rolled-up day.
        assert_eq!(store.prune(DEFAULT_RAW_RETENTION_DAYS).unwrap(), 0);

        let key = snap.environments[0].accounts[0].stable_key();
        let rollup = store.daily_rollup(&key, 90).unwrap();
        assert_eq!(rollup.len(), 1);
        assert_eq!(rollup[0].sample_count, 1);
    }

    // -- summary ----------------------------------------------------------------

    #[test]
    fn summary_reports_weekly_average_and_hundred_pct_hits() {
        let (_dir, store) = store();

        let now = Utc::now();
        let samples = [
            (now - Duration::days(1), 50.0),
            (now - Duration::days(2), 100.0),
            (now - Duration::days(3), 30.0),
        ];
        for (ts, pct) in samples {
            let ts_str = ts.to_rfc3339_opts(SecondsFormat::Secs, true);
            let snap = snapshot_for(Some("org-1"), "a@x.com", &ts_str, 1.0, pct, &[]);
            store.record(&snap).unwrap();
        }

        let summary = store.summary(30).unwrap();
        assert!((summary.weekly_average_pct - (50.0 + 100.0 + 30.0) / 3.0).abs() < 1e-9);
        assert_eq!(summary.times_at_100_pct, 1);
        assert!(summary.busiest_weekday.is_some());
    }
}
