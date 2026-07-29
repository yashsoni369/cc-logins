#!/usr/bin/env node
// Drafts a CHANGELOG.md section for a version from the conventional-commit
// subjects since the last tag.
//
//   pnpm changelog 0.2.0
//   pnpm changelog 0.2.0 --dry-run
//
// This is a first draft, not the finished entry. Commit subjects are written
// for reviewers and changelog entries are read by users, and the two rarely
// want the same words — the point is to stop anything being forgotten, not to
// skip the writing. Edit the section it produces, then run `pnpm release`.
//
// Deliberately separate from release.mjs so there is an editing step between
// generating and tagging.

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')

const args = process.argv.slice(2)
const dryRun = args.includes('--dry-run')
const force = args.includes('--force')
const version = args.find((a) => !a.startsWith('-'))

function die(message, hint) {
  console.error(`\n  error: ${message}`)
  if (hint) console.error(`  ${hint}`)
  console.error('')
  process.exit(1)
}

function git(...argv) {
  return execFileSync('git', argv, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim()
}

if (!version) {
  die('no version given', 'usage: pnpm changelog <version> [--dry-run] [--force]')
}

const changelogPath = path.join(root, 'CHANGELOG.md')
const changelog = readFileSync(changelogPath, 'utf8')

if (changelog.includes(`## [${version}]`) && !force) {
  die(
    `CHANGELOG.md already has a "## [${version}]" section`,
    'edit it directly, or pass --force to add a second one and merge them by hand.',
  )
}

// Range: everything since the most recent tag. With no tags at all, the whole
// history is the first release.
let range = ''
let since = 'the beginning of history'
try {
  const lastTag = git('describe', '--tags', '--abbrev=0')
  range = `${lastTag}..HEAD`
  since = lastTag
} catch {
  // No tags yet.
}

// --no-merges: a merge subject names the branch, never the change. The commits
// it brought in are in the range on their own.
const log = git('log', '--no-merges', '--format=%s', ...(range ? [range] : []))
const subjects = log ? log.split('\n').filter(Boolean) : []

if (!subjects.length) {
  die(`no commits since ${since}`, 'nothing to write a changelog section from.')
}

// Conventional commits: `type(scope)!: subject`. Types that describe a change a
// user would notice map to Keep a Changelog headings; the rest are internal.
const HEADINGS = {
  feat: 'Added',
  fix: 'Fixed',
  perf: 'Changed',
  refactor: 'Changed',
  revert: 'Changed',
  style: null,
  docs: null,
  test: null,
  chore: null,
  ci: null,
  build: null,
}

const sections = { Added: [], Changed: [], Fixed: [], Security: [] }
const uncategorised = []
const skipped = []

for (const subject of subjects) {
  const match = subject.match(/^(\w+)(\([^)]*\))?(!)?:\s*(.+)$/)

  if (!match) {
    uncategorised.push(subject)
    continue
  }

  const [, type, , breaking, text] = match
  const key = type.toLowerCase()

  if (!Object.hasOwn(HEADINGS, key)) {
    uncategorised.push(subject)
    continue
  }

  const heading = HEADINGS[key]
  if (heading === null) {
    skipped.push(subject)
    continue
  }

  // A `!` marks a breaking change, which at 0.x bumps the minor. Worth
  // surfacing loudly rather than filing it as an ordinary entry.
  const entry = breaking ? `**Breaking.** ${text}` : text
  sections[breaking ? 'Changed' : heading].push(entry)
}

const lines = [`## [${version}]`, '']

for (const [heading, entries] of Object.entries(sections)) {
  if (!entries.length) continue
  lines.push(`### ${heading}`, '')
  for (const entry of entries) lines.push(`- ${entry}`)
  lines.push('')
}

// Never dropped silently: an unparsed subject is one a human has to look at,
// and quietly discarding it is how a real change misses the release notes.
if (uncategorised.length) {
  lines.push(
    '### Uncategorised — triage these before releasing',
    '',
    ...uncategorised.map((s) => `- ${s}`),
    '',
  )
}

const section = lines.join('\n')

console.log(`\n  ${subjects.length} commit(s) since ${since}\n`)
console.log(section)

if (skipped.length) {
  console.log(`  omitted ${skipped.length} internal commit(s) (docs, test, chore, ci, build, style):`)
  for (const s of skipped) console.log(`    ${s}`)
  console.log('')
}

if (dryRun) {
  console.log('  --dry-run: CHANGELOG.md not written.\n')
  process.exit(0)
}

// Insert above the newest existing version heading, or above the link
// references if this is the first section.
const anchor = changelog.search(/^## \[\d/m)
const insertAt = anchor === -1 ? changelog.search(/^\[[^\]]+\]: /m) : anchor

if (insertAt === -1) {
  die('could not find where to insert', 'CHANGELOG.md has neither a version heading nor link references.')
}

writeFileSync(
  changelogPath,
  `${changelog.slice(0, insertAt)}${section}\n${changelog.slice(insertAt)}`,
)

console.log(`  Written to CHANGELOG.md.

  Edit it into something a user would want to read, then:

    pnpm release ${version} --allow-branch --no-tag
`)
