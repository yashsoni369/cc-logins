# Releasing

Maintainer notes. Releases are cut by pushing a tag; nothing publishes itself.

## The short version

```
# 1. write the changelog section for the version first
# 2. then:
pnpm release 0.2.0
git push origin main v0.2.0
# 3. review the draft on the releases page and publish it
```

## What the pieces do

Three files carry the version — `package.json`, `src-tauri/tauri.conf.json`, and
`src-tauri/Cargo.toml` — plus `src-tauri/Cargo.lock`, which records the crate's
own version too. Tauri stamps `tauri.conf.json`'s value into every installer, so
drift between them ships wrong version numbers rather than failing anything
loudly. CI checks the three agree on every push; the release workflow additionally
checks they match the tag.

`pnpm release <version>` is what keeps them in step. It bumps all four files,
dates the changelog section, resets `[Unreleased]`, fixes the changelog link
references, then commits and tags locally. It deliberately stops there — pushing
the tag is what starts the build, and that stays a deliberate act.

It refuses to run on a dirty tree, off `main`, behind `origin/main`, over an
existing tag, or without a changelog section for the version. Use `--dry-run` to
preview the edits (there the checkout checks soften to warnings), and
`--allow-branch` to release from somewhere other than `main`.

## Step 1 — write the changelog first

`CHANGELOG.md` is the source of the release notes, not an afterthought. Add a
section for the version before running anything:

```markdown
## [0.2.0]

### Added

- ...
```

Leave the date off; the script fills it in. The release workflow reads this
section and uses it as the release body, and **fails the release if the section
is missing** — an undocumented release stops at the version check rather than
shipping with an empty description.

## Step 2 — cut it

```
pnpm release 0.2.0 --dry-run    # optional preview
pnpm release 0.2.0
git show v0.2.0                 # review the commit and tag
```

To undo anything before pushing:

```
git tag -d v0.2.0 && git reset --hard HEAD~1
```

## Step 3 — push, which starts the build

```
git push origin main v0.2.0
```

The tag push triggers `.github/workflows/release.yml`, which:

1. Verifies the tag matches all three version fields and extracts the changelog
   section for the release body.
2. Builds on three runners — Windows, `macos-latest` (one universal binary
   covering Apple Silicon and Intel), and `ubuntu-22.04` — and attaches the
   installers to a **draft** release.
3. Downloads the attached assets and adds `SHA256SUMS.txt`.

Asset names omit the version (`CC-Logins-[platform]-[arch][setup][ext]`) so that
`releases/latest/download/<file>` permalinks stay valid across releases.

## Step 4 — publish

Nothing is visible to users until you publish. Check that every platform's
installer is present and that `SHA256SUMS.txt` lists them all — a failed build
leg leaves an incomplete draft rather than failing loudly — then publish from the
releases page.

If a build leg fails, fix it and re-run the workflow from the Actions tab. The
checksum step replaces its previous output rather than colliding with it.

## Version numbers

Semver, with major pinned at 0. At `0.x` the public contract is the on-disk shape
— the account vault layout, the settings file, the usage-history schema — plus
the supported OS matrix. A breaking change to any of those bumps the minor.

## Signing

Builds are unsigned, so Windows SmartScreen and macOS Gatekeeper warn on them.
`SHA256SUMS.txt` is the substitute: it proves a download matches what the workflow
built, though not that the release itself is trustworthy. The signing secrets
`release.yml` expects are listed, commented out, at the `tauri-action` step —
none of them exist in the repository yet. See the README's "Signing" section and
PLAN.md §7 for the plan.
