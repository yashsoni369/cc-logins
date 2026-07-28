//! Crash-context logging for panics.
//!
//! A GUI process has no console attached in the normal case, so an unhandled
//! panic is otherwise invisible — exactly the failure mode `init_logging` in
//! `lib.rs` already fixed for `log::warn!`/`log::error!` calls (see that
//! function's doc comment: "a usage-fetch failure became invisible and
//! surfaced as a wrong number on screen"). Panics bypass the `log` crate
//! entirely, though — they go through `std::panic`, not `log`, so that fix
//! does not cover them. This module closes that specific gap: it installs a
//! panic hook that appends a self-contained crash report (timestamp, thread,
//! panic message, source location, and a best-effort backtrace) to the same
//! log file [`crate::log_path`] already writes ordinary logs to, so a user
//! attaching `app.log` to a bug report carries crash context too, without a
//! second file to ask for.
//!
//! # Why a panic hook alone is not the whole story
//!
//! The panic machinery always runs the installed hook before deciding what
//! happens next — that part is unconditional, so this hook fires regardless
//! of the crate's `panic` profile setting. What happens *after* the hook
//! returns is not unconditional, though, and that is where this crate's
//! current `[profile.release] panic = "abort"` (see `Cargo.toml`) matters
//! more than its name suggests:
//!
//! - Every `std::panic::catch_unwind` in `poller.rs` — and there are several,
//!   deliberately wrapping `decide()`, tray rendering, and history recording
//!   specifically so one bad tick cannot take the whole daemon down, per that
//!   module's own "Never panics" doc section — **cannot catch anything in a
//!   release build** while `panic = "abort"` is set. `catch_unwind` only
//!   works when unwinding is the chosen strategy; under `abort` the process
//!   terminates immediately after this hook returns, regardless of any
//!   `catch_unwind` wrapper further up the stack. None of that machinery is
//!   wrong — it is just inert in every release build shipped today.
//! - Destructors do not run either, which is *why* `login::sweep_stale_login_dirs`
//!   exists as a startup backstop for the isolated-login temp directory (see
//!   that function's own doc comment).
//!
//! cc-switch (farion1231/cc-switch, MIT) reaches the same conclusion for the
//! same reason: its `Cargo.toml` sets `panic = "unwind"` in
//! `[profile.release]`, with a comment translating to "use unwind so the
//! panic hook can capture a backtrace — abort terminates immediately and
//! nothing can catch it." This module's hook is deliberately written to work
//! correctly either way — it never assumes unwinding happens — but see
//! `RESILIENCE.md` (§1) for the recommendation to actually switch this
//! crate's profile to match, which is what would let `poller.rs`'s own
//! design do what its doc comments already say it does.
//!
//! # Attribution
//!
//! The overall shape (a wrapped previous hook, a human-readable bordered
//! report, defensively-formatted timestamp so a panic during formatting
//! cannot suppress the report itself) follows the same idea as cc-switch's
//! `src-tauri/src/panic_hook.rs`. The code below is a fresh implementation
//! for this crate's shape (one shared `app.log` instead of a separate
//! `crash.log`, no rotation here — see `RESILIENCE.md` for why), not a port;
//! the `payload().downcast_ref::<&str>() / ::<String>()` pair is the standard
//! idiom for reading a panic payload and is not particular to either
//! project.

use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::Path;
use std::sync::Mutex;

/// Serializes concurrent panics (from different threads) writing to the same
/// file. A poisoned lock still yields its guard via `into_inner` rather than
/// panicking again — panicking a second time *inside a panic hook* would
/// abort the process before even this first report is flushed, which is
/// strictly worse than a small chance of two reports interleaving.
static CRASH_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Extract a human-readable panic message from `info`.
///
/// Covers the two payload shapes `panic!`, `.unwrap()`, and `.expect()`
/// actually produce (`&'static str` for a literal, `String` for anything
/// built with `format!`/`panic!("{}", ...)`); anything else falls back to
/// `PanicHookInfo`'s own `Display`, which still names a source location even
/// when the payload itself is some other type entirely.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        info.to_string()
    }
}

/// `"file:line:column"`, or a fixed placeholder when the compiler didn't
/// attach one (rare, but `Location` is technically optional on the API).
fn panic_location(info: &PanicHookInfo<'_>) -> String {
    match info.location() {
        Some(loc) => format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        None => "unknown location".to_string(),
    }
}

/// Build one crash report block from already-extracted, plain data.
///
/// Kept separate from [`install`] so the formatting itself can be unit
/// tested without constructing a real `PanicHookInfo` — the standard library
/// deliberately exposes no public constructor for it, so anything that takes
/// one directly can only be exercised by triggering an actual panic under an
/// installed hook (see the `panic_message_and_location_are_extracted_from_a_real_panic`
/// test below for that half of the coverage).
fn format_crash_report(message: &str, location: &str, thread: &str, backtrace: &str) -> String {
    // The timestamp is formatted defensively for the same reason
    // `panic_message` never trusts a payload's `Display` blindly: this
    // function runs from inside a panic hook, and a second panic here (say,
    // from a `chrono` edge case) would only ever be caught if the crate is
    // built with `panic = "unwind"` — under the current `"abort"` profile it
    // would simply kill the process with no report written at all, on top
    // of the crash the hook was trying to describe in the first place.
    let timestamp = std::panic::catch_unwind(|| {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
    })
    .unwrap_or_else(|_| "unknown time".to_string());

    format!(
        "\n\
         ==================== CRASH ====================\n\
         time:     {timestamp}\n\
         version:  {}\n\
         thread:   {thread}\n\
         message:  {message}\n\
         location: {location}\n\
         backtrace:\n{backtrace}\n\
         ================================================\n",
        env!("CARGO_PKG_VERSION"),
    )
}

/// Best-effort append of `entry` to `path`. Never propagates a failure and
/// never panics: this runs inside a panic hook, where any panic would abort
/// the process before this report is flushed (see the timestamp comment in
/// [`format_crash_report`]) — silently failing to log is strictly better
/// than that.
fn append_report(path: &Path, entry: &str) {
    let _guard = CRASH_WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(entry.as_bytes());
        let _ = file.flush();
    }
}

/// Install the panic hook.
///
/// Call once, as early as possible in `run()` — ideally before
/// `init_logging()`, so a panic during logging setup itself is still
/// captured (this function creates its own parent directory on demand, so it
/// does not depend on `init_logging` having run first). Wraps rather than
/// replaces whatever hook was previously installed (Rust's own default,
/// which prints to stderr) and always calls it afterwards, so normal `cargo
/// run` / dev-console behaviour is unchanged — this only adds the file
/// write.
pub fn install() {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let location = panic_location(info);
        let thread = std::thread::current().name().unwrap_or("<unnamed>").to_string();

        // `force_capture` ignores `RUST_BACKTRACE` and always captures. A
        // release build with `strip = true` (see Cargo.toml) will often
        // yield addresses rather than symbol names here, but the message and
        // location above are captured verbatim from the source and do not
        // depend on symbols surviving stripping, so the report stays useful
        // even when this part is not.
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();

        let entry = format_crash_report(&message, &location, &thread, &backtrace);
        append_report(&crate::log_path(), &entry);

        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;
    use std::sync::{Arc, Mutex as StdMutex};

    #[test]
    fn format_crash_report_includes_every_field() {
        let report = format_crash_report("boom", "src/foo.rs:12:5", "main", "frame0\nframe1");
        assert!(report.contains("boom"));
        assert!(report.contains("src/foo.rs:12:5"));
        assert!(report.contains("main"));
        assert!(report.contains("frame0"));
        assert!(report.contains("frame1"));
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.contains("CRASH"));
    }

    #[test]
    fn append_report_creates_parent_dir_and_appends_rather_than_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("app.log");

        append_report(&path, "first\n");
        append_report(&path, "second\n");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));
        assert!(
            contents.find("first").unwrap() < contents.find("second").unwrap(),
            "second call must append after the first, not overwrite it"
        );
    }

    #[test]
    fn append_report_degrades_silently_on_an_unwritable_path() {
        // An embedded NUL byte is rejected by the OS path APIs on every
        // platform Rust targets here, so this reliably exercises the
        // failure path without depending on filesystem permissions being
        // set up a particular way. The only assertion is that this does not
        // panic — a panic hook that itself panics aborts the process with no
        // report written at all, which is exactly the failure mode this
        // function exists to avoid.
        let bogus = Path::new("\0invalid\0path\0app.log");
        append_report(bogus, "entry\n");
    }

    #[test]
    fn panic_message_and_location_are_extracted_from_a_real_panic() {
        // `PanicHookInfo` has no public constructor, so the only way to
        // exercise `panic_message`/`panic_location` against a real instance
        // is to observe one from inside an installed hook. This mutates the
        // process-global panic hook for the duration of the test only, and
        // restores the previous one before returning — a `Drop` guard is
        // avoided in favour of an explicit restore right after
        // `catch_unwind`, so the window during which the global hook is
        // replaced is as short as possible.
        //
        // The captured value is only recorded when the message contains a
        // random-looking marker unique to this test, so an unrelated panic
        // on another test thread racing this narrow window cannot be
        // mistaken for this test's own panic.
        let captured: Arc<StdMutex<Option<(String, String)>>> = Arc::new(StdMutex::new(None));
        let captured_in_hook = captured.clone();

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = panic_message(info);
            if msg.contains("resilience-test-marker-9f3c") {
                *captured_in_hook.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some((msg, panic_location(info)));
            }
        }));

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            panic!("resilience-test-marker-9f3c: boom");
        }));

        std::panic::set_hook(previous);

        assert!(result.is_err());
        let (msg, loc) = captured
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("the hook should have observed the marked panic");
        assert!(msg.contains("resilience-test-marker-9f3c"), "got {msg}");
        assert!(loc.contains("resilience.rs"), "got {loc}");
    }
}
