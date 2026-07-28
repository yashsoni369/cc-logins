# Contributing

Single maintainer, pre-1.0, and the app handles OAuth credentials. That shapes
what gets merged.

**Open an issue before writing anything large.** Bug fixes and small,
self-contained changes are welcome unannounced. Unsolicited large refactors,
dependency swaps, restyles, and "modernisations" may be closed without review —
not because they are bad, but because reviewing them costs more than the change
is worth to a project of this size.

## Setup

Prerequisites: Rust 1.81+, Node 20.19+ or 22.12+, pnpm 10.14.0, plus Tauri's
platform build tools — MSVC Build Tools on Windows, Xcode Command Line Tools on
macOS, `webkit2gtk` and friends on Linux (see
[Tauri's prerequisites](https://v2.tauri.app/start/prerequisites/)).

```
pnpm install
pnpm tauri dev
```

On Windows, `cargo` often isn't on `PATH` in the shell you already have open
after installing rustup. Open a new terminal, or add `%USERPROFILE%\.cargo\bin`
yourself.

## Before opening a PR

Exactly what CI runs, nothing extra:

```
pnpm exec tsc --noEmit
pnpm build
cd src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

CI runs the Rust jobs on all three platforms, because real platform-specific
code sits behind `#[cfg(...)]`. A green run on your OS checks a third of it.

## Test safety — read this before touching tests

A run of this suite once resolved a real credential backup directory and
destroyed two logged-in accounts. Two guards exist because of that:

- `guard_real_store()` in `src-tauri/src/test_support.rs` panics if a test
  resolves a path outside a temp directory. It is called on every resolution
  under `cfg(test)`, so it is not opt-in.
- `src-tauri/tests/differential.rs` may invoke only its explicitly allowlisted
  read-only commands.

Never weaken either one, and never point a test at a real credential store. If
a guard fires, the test's environment override didn't take effect — fix the
test. Relaxing the assertion to go green is the exact trade that caused the
data loss.

## Other notes

- Never paste tokens, credential files, or `.credentials.json` contents into an
  issue, PR, or screenshot.
- Suspected vulnerabilities go to [SECURITY.md](SECURITY.md), not a public
  issue.
- Contributions are MIT licensed, same as the project.
