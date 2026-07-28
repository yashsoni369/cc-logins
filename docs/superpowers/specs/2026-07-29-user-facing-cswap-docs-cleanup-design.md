# User-facing cswap documentation cleanup

## Status

Proposed on 2026-07-29.

## Goal

Present CC Logins as an independent application without making `cswap`
compatibility part of its public positioning.

## Scope

- Remove relationship and compatibility explanations from `README.md`.
- Replace `cswap`-specific release notes with descriptions of CC Logins'
  own behavior in `CHANGELOG.md`.
- Remove external-tool testing instructions and incident wording from
  `CONTRIBUTING.md`, while preserving the underlying credential-test safety
  rules.
- Remove `cswap` references from the public issue and pull-request templates.
- Retain one short attribution statement in the README.
- Retain the required upstream copyright and MIT notice in `LICENSE`.

## Out of scope

- Product behavior, runtime code, tests, and CI behavior.
- Source-code attribution and implementation comments.
- Historical plans and specifications.
- The in-app About screen and bundle metadata.

## Verification

- Search the selected user-facing files for `cswap`, `claude-swap`, and
  `claude_swap`.
- Confirm the README contains only the agreed short attribution.
- Confirm `LICENSE` remains unchanged.
- Review the diff to ensure no product or CI files changed.
