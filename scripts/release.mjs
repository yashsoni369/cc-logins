#!/usr/bin/env node
// Prepares a release: bumps the four files that carry the version, dates the
// changelog section, then commits and tags locally. It deliberately does not
// push — pushing the tag is what triggers the build, and that stays a
// deliberate act by a human.
//
//   pnpm release 0.1.0
//   pnpm release 0.2.0 --dry-run
//
// CI enforces the same invariants this script maintains (ci.yml checks the
// three versions agree; release.yml checks they match the tag and that the
// changelog has a section for it), so a hand-edited release still works. This
// just makes getting it right the default instead of a four-file ritual.

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const REPO = 'https://github.com/yashsoni369/cc-logins'

const args = process.argv.slice(2)
const dryRun = args.includes('--dry-run')
const allowBranch = args.includes('--allow-branch')
const version = args.find((a) => !a.startsWith('-'))

function die(message, hint) {
  console.error(`\n  error: ${message}`)
  if (hint) console.error(`  ${hint}`)
  console.error('')
  process.exit(1)
}

// Checks about the state of the checkout rather than the release itself. A dry
// run only previews the file edits, so these become warnings there — otherwise
// you could not preview a bump without first cleaning the tree.
function require_(condition, message, hint) {
  if (condition) return
  if (dryRun) console.warn(`  warning: ${message}`)
  else die(message, hint)
}

function git(...argv) {
  // stderr is piped rather than inherited so the callers that expect a command
  // to fail (a missing tag, no upstream branch) stay quiet.
  return execFileSync('git', argv, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim()
}

// Rewrites `find` -> `replace` in a file, refusing to continue unless it
// matched exactly once. A silent zero-match here would produce a release whose
// installers carry the old version, which is the exact failure the version
// guards in CI exist to catch — better to stop now than at tag time.
const edits = []
function stage(relPath, find, replace) {
  const file = path.join(root, relPath)
  const before = readFileSync(file, 'utf8')
  const matches = before.split(find).length - 1
  if (matches !== 1) {
    die(
      `expected exactly one occurrence of ${JSON.stringify(find)} in ${relPath}, found ${matches}`,
      'the file layout changed; update scripts/release.mjs to match.',
    )
  }
  edits.push({ file, relPath, after: before.replace(find, replace) })
}

function commit(relPath, contents) {
  edits.push({ file: path.join(root, relPath), relPath, after: contents })
}

// ---- validate input -------------------------------------------------------

if (!version) {
  die('no version given', 'usage: pnpm release <version> [--dry-run] [--allow-branch]')
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+(\.[0-9A-Za-z.]+)*)?$/.test(version)) {
  die(`"${version}" is not a valid semver version`, 'expected something like 0.2.0 or 1.0.0-rc.1')
}

const tag = `v${version}`
const current = JSON.parse(readFileSync(path.join(root, 'package.json'), 'utf8')).version

if (version === current) {
  die(`package.json is already at ${version}`, 'pick a new version, or check out an earlier commit.')
}

// ---- validate the working tree -------------------------------------------

const branch = git('rev-parse', '--abbrev-ref', 'HEAD')
require_(
  branch === 'main' || allowBranch,
  `on branch "${branch}", not main`,
  'pass --allow-branch to release from here anyway.',
)

require_(
  !git('status', '--porcelain'),
  'working tree has uncommitted changes',
  'commit or stash them first — the release commit should only carry the version bump.',
)

let tagExists = true
try {
  git('rev-parse', '-q', '--verify', `refs/tags/${tag}`)
} catch {
  tagExists = false // rev-parse exits non-zero when the tag is absent
}
require_(
  !tagExists,
  `tag ${tag} already exists`,
  `delete it with \`git tag -d ${tag}\` if it was never pushed.`,
)

// A release built from a stale main silently omits whatever landed upstream.
let behind = null
try {
  git('fetch', 'origin', '--quiet')
  behind = git('rev-list', '--count', `HEAD..origin/${branch}`)
} catch {
  console.warn('  warning: could not compare against origin — using local state alone.')
}
if (behind !== null) {
  require_(
    behind === '0',
    `local ${branch} is ${behind} commit(s) behind origin/${branch}`,
    'run `git pull --ff-only` first.',
  )
}

// ---- stage the version bump ----------------------------------------------

stage('package.json', `"version": "${current}"`, `"version": "${version}"`)
stage('src-tauri/tauri.conf.json', `"version": "${current}"`, `"version": "${version}"`)
stage('src-tauri/Cargo.toml', `\nversion = "${current}"`, `\nversion = "${version}"`)

// Cargo.lock records the workspace crate's own version too, and a stale entry
// makes `--frozen-lockfile`-style builds fail. Edited directly rather than by
// running cargo, so this script needs no Rust toolchain.
stage(
  'src-tauri/Cargo.lock',
  `name = "cc-logins"\nversion = "${current}"`,
  `name = "cc-logins"\nversion = "${version}"`,
)

// ---- stage the changelog --------------------------------------------------

const changelogPath = path.join(root, 'CHANGELOG.md')
const changelog = readFileSync(changelogPath, 'utf8')
const lines = changelog.split('\n')

const headingIndex = lines.findIndex((line) => line.startsWith(`## [${version}]`))
if (headingIndex === -1) {
  die(
    `CHANGELOG.md has no "## [${version}]" section`,
    'add one describing this release before tagging — release.yml uses it as the release body and fails without it.',
  )
}

// Local date, not toISOString(): east of UTC that reports yesterday for most of
// the working day, dating the release before the commit that made it.
const now = new Date()
const today = [
  now.getFullYear(),
  String(now.getMonth() + 1).padStart(2, '0'),
  String(now.getDate()).padStart(2, '0'),
].join('-')
lines[headingIndex] = `## [${version}] - ${today}`

// The Unreleased section describes what is not yet shipped. Once this version
// ships, whatever it said about this version is wrong.
const unreleasedIndex = lines.findIndex((line) => line.startsWith('## [Unreleased]'))
if (unreleasedIndex !== -1 && unreleasedIndex < headingIndex) {
  lines.splice(unreleasedIndex + 1, headingIndex - unreleasedIndex - 1, '', 'Nothing yet.', '')
}

let updated = lines.join('\n')

// Point the Unreleased compare link at the new tag, and give this version its
// own link so the heading resolves. `previous` is the next version heading
// below this one, which is the release this one is diffed against.
const previous = updated
  .split('\n')
  .filter((line) => /^## \[\d/.test(line))
  .map((line) => line.match(/^## \[([^\]]+)\]/)[1])
  .find((v) => v !== version)

const versionLink = previous
  ? `[${version}]: ${REPO}/compare/v${previous}...${tag}`
  : `[${version}]: ${REPO}/releases/tag/${tag}`

if (updated.includes('[Unreleased]: ')) {
  updated = updated.replace(
    /\[Unreleased\]: .*/,
    `[Unreleased]: ${REPO}/compare/${tag}...HEAD\n${versionLink}`,
  )
} else {
  updated = `${updated.trimEnd()}\n\n[Unreleased]: ${REPO}/compare/${tag}...HEAD\n${versionLink}\n`
}

commit('CHANGELOG.md', updated)

// ---- apply ----------------------------------------------------------------

console.log(`\n  ${current} -> ${version}   (tag ${tag}, dated ${today})\n`)
for (const edit of edits) console.log(`    ${edit.relPath}`)

if (dryRun) {
  console.log('\n  --dry-run: nothing written.\n')
  process.exit(0)
}

for (const edit of edits) writeFileSync(edit.file, edit.after)

git('add', ...edits.map((e) => path.relative(root, e.file)))
git('commit', '-m', `release: ${version}`)
git('tag', '-a', tag, '-m', `CC Logins ${version}`)

console.log(`
  Committed and tagged locally. Nothing has been pushed.

  Review it:

    git show ${tag}

  Then push, which starts the release build:

    git push origin ${branch} ${tag}

  The build stages a draft release with the Windows installer, a universal
  macOS .dmg, the Linux .AppImage/.deb, and SHA256SUMS.txt. Review the assets
  on the releases page and publish it there.

  To undo before pushing:

    git tag -d ${tag} && git reset --hard HEAD~1
`)
