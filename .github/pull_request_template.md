## What and why

<!-- One or two sentences. Link the issue: Fixes #123 -->

## Screenshots

Required for any visible change. Delete this section only if nothing on screen
moved. Tray icon changes count — screenshot the tray.

| Before | After |
| ------ | ----- |
|        |       |

## How it was tested

<!-- Which OS, and what you actually clicked. "CI is green" is not a test of a GUI. -->

## Checks

These are exactly what CI runs. Run them locally first.

- [ ] `pnpm install --frozen-lockfile`
- [ ] `pnpm exec tsc --noEmit`
- [ ] `pnpm build`
- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cd src-tauri && cargo test --all-features`

CI runs the Rust jobs on Linux, Windows and macOS because platform-specific
code sits behind `#[cfg(...)]`; a green local run on one OS proves one third of
it.

## Credentials and tests

- [ ] No token, credential file, or `.credentials.json` content appears in the
      diff, the screenshots, or this description.
- [ ] No test points at a real credential store, and `guard_real_store()` in
      `src-tauri/src/test_support.rs` is unchanged or strengthened — never
      relaxed.
- [ ] Any new case in `src-tauri/tests/differential.rs` uses only the
      read-only `cswap list` / `cswap status` subcommands.
