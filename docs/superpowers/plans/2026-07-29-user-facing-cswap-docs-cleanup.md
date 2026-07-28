# User-facing cswap documentation cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for progress tracking.

**Goal:** Present CC Logins as an independent product in current user-facing repository documentation while retaining one concise upstream attribution and the required license notice.

**Architecture:** This is a documentation-only cleanup. Rewrite current product behavior in CC Logins terms, remove public prompts that position `cswap` as part of the user workflow, and leave implementation, tests, CI, historical engineering records, in-app metadata, and licensing untouched.

**Tech Stack:** Markdown, GitHub issue-form YAML, PowerShell search commands, Git.

**Global Constraints:** Work only in the `feature/release-blockers` worktree. Preserve all existing dirty changes. Do not modify `LICENSE`, product code, tests, CI, historical plans/specs, in-app About text, or bundle metadata. Keep exactly one short `claude-swap` attribution in `README.md`.

---

## Task 1: Reframe the primary public documentation

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Remove the README relationship section**

  Delete the `Relationship to the cswap CLI` section and its interoperability positioning. Preserve the adjacent transaction-recovery and credential-storage explanations as CC Logins behavior.

- [ ] **Step 2: Reduce README attribution to one concise mention**

  Keep the License section's linked upstream attribution, shortening it if needed while retaining author, MIT status, and the pointer to `LICENSE`.

- [ ] **Step 3: Rewrite changelog entries in product-native language**

  Describe cross-process locking, credential provenance, usage attribution, and OAuth recovery as CC Logins capabilities. Preserve every behavioral guarantee; remove only comparisons to or compatibility claims about `cswap`.

- [ ] **Step 4: Generalize contributor safety guidance**

  Retain the real credential-loss warning and both safety rules, but refer to a real credential backup/store and read-only differential commands without naming an external product.

## Task 2: Clean public contribution prompts

**Files:**
- Modify: `.github/pull_request_template.md`
- Modify: `.github/ISSUE_TEMPLATE/feature_request.yml`

- [ ] **Step 1: Make the PR safety checklist implementation-neutral**

  Require differential-test cases to remain read-only without naming specific external commands.

- [ ] **Step 2: Make the feature-request prompt product-native**

  Ask about existing settings or workarounds without suggesting an external CLI.

## Task 3: Verify scope and outcome

**Files:**
- Verify: `README.md`
- Verify: `CHANGELOG.md`
- Verify: `CONTRIBUTING.md`
- Verify: `.github/pull_request_template.md`
- Verify: `.github/ISSUE_TEMPLATE/feature_request.yml`
- Verify unchanged: `LICENSE`

- [ ] **Step 1: Search the edited user-facing files**

  Run:

  ```powershell
  rg -n -i "cswap|claude-swap" README.md CHANGELOG.md CONTRIBUTING.md .github/pull_request_template.md .github/ISSUE_TEMPLATE/feature_request.yml
  ```

  Expected: exactly one concise attribution in `README.md`; no matches in the other four files.

- [ ] **Step 2: Confirm the license notice was not edited**

  Run:

  ```powershell
  git diff --exit-code -- LICENSE
  ```

  Expected: exit code 0 and no output.

- [ ] **Step 3: Review the scoped diff and whitespace**

  Run:

  ```powershell
  git diff --check
  git diff -- README.md CHANGELOG.md CONTRIBUTING.md .github/pull_request_template.md .github/ISSUE_TEMPLATE/feature_request.yml
  ```

  Expected: no whitespace errors; changes are limited to the approved wording cleanup and preserve the documented behavioral and safety guarantees.

- [ ] **Step 4: Confirm no unrelated file was added to this task's staged scope**

  If committing, stage only the five edited user-facing files plus this plan. Do not stage any existing source or test changes.
