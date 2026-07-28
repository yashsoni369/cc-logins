# Transaction recovery

Account activation changes three independently stored resources: Claude Code's active credential
backend, Claude's global config, and this app's `sequence.json`. Atomic replacement prevents a torn
individual file; it cannot make those three writes atomic as a group. CC Logins therefore treats a
switch as a recoverable transaction.

## Compatibility boundary

The lock order is fixed:

1. `<cswap-store>/.lock`, only when a real cswap store already exists;
2. `<CLAUDE_CONFIG_DIR>/.oauth_refresh.lock`;
3. the sibling legacy credential lock (normally `~/.claude.lock`);
4. the global-config lock (normally `~/.claude.json.lock`);
5. `<cc-logins-account-vault>/.lock`.

The Claude directory-lock paths and timing match Claude Code 2.1.218 as represented by pinned cswap
commit `65d208081a4985b9fd1786bc258d5172d196bee2`: credential locks become stale after 60 seconds and
the config lock after 10 seconds. CC Logins refreshes lock mtimes every three seconds and never holds
these locks across OAuth or other network work.

## Credential provenance

Before a switch takes any mutation lock, CC Logins resolves the live OAuth profile. Under the lock,
that verdict is accepted only if the live credential generation is unchanged. Bytes positively
attributed to another or recycled account are saved to the unclaimed safety store and are never
written into the configured outgoing slot. Wiped token fields likewise cannot replace a usable slot
backup. If the profile oracle is unavailable or incomplete, switching keeps cswap's fail-open rule
for legitimate local token rotations.

Usage collection applies the same ownership boundary. A stored-backup lineage match is sufficient;
otherwise, a successful usage response is assigned to the configured slot only when the profile
oracle does not prove it foreign. Definitive lineage verdicts are cached, but endpoint failures and
partial identities are not, so later collection passes retry them.

## Durable artifacts

The private account vault contains these temporary recovery artifacts:

- `switch-journal.json`: schema-versioned metadata, target identity, phase, locators, lengths, and
  SHA-256 hashes. It contains no credential or config plaintext.
- `switch-recovery/<transaction-id>/`: protected before-images and the captured outgoing generation.
  Windows uses DPAPI; macOS uses Keychain when available and records an honest 0600 file fallback;
  other platforms use an honestly labelled 0600 file envelope.
- synced sibling `*.stage` files beside regular-file targets until they are installed or cleaned.

Journal phases are `Prepared`, `ActiveCredentialInstalled`, `GlobalConfigInstalled`,
`SequenceInstalled`, and `Committed`. A phase becomes visible only after its journal replacement is
durable.

## Recovery behavior

- Any noncommitted phase restores exact prior presence and bytes in reverse write order, verifies
  every backend, and preserves the outgoing account's matching credential/config generation.
- `Committed` never rolls back merely because cleanup was interrupted. Startup verifies the target
  hashes, then finishes cleanup.
- Malformed/unknown journals, tampered or missing before-images, lock failure, or incomplete restore
  fail closed. The app still opens, but manual and automatic switching return/report
  `recoveryRequired` and no new mutation starts.
- A hard crash leaves proper-lockfile directories behind. They are not stolen while fresh. Startup
  retries recovery in the background long enough to cross the canonical 60-second credential-lock
  staleness boundary.

Recovery is idempotent: rerunning it after a partial rollback, partial cleanup, or a second process
termination converges on the same coherent state.

## Supported filesystem boundary

The durability claims apply to ordinary local filesystems supported by the host OS (for example
NTFS, APFS, and ext4) and to the platform credential backend. Network filesystems, sync-provider
virtual drives, and filesystems that do not honor the documented rename/write-through semantics are
outside this guarantee.

For manual diagnostics, preserve `switch-journal.json`, the referenced `switch-recovery` directory,
and the application log together. Do not edit or delete individual recovery artifacts: hashes and
presence flags intentionally make partial manual changes fail closed.
