# SignPath Foundation application

Working notes for applying to the [SignPath Foundation](https://signpath.org/) for free
Windows code signing. Nothing here is a claim that the project is signed — it is not.
The code signing policy at the bottom is a **draft to publish only after approval**.

Apply at <https://signpath.org/apply> once the checklist below is green.

## Why this route

Windows signing has three realistic options and two of them are closed:

| Option | Status for this project |
| --- | --- |
| Microsoft Store (MSIX) | Would remove the warning entirely, but Tauri emits only NSIS and MSI — MSIX needs third-party packaging. Open, but real work. |
| Azure Artifact Signing | **Closed.** ~$10/month, but individual developers are limited to the USA and Canada. |
| OV certificate from a CA | Open. $150–300/year plus a hardware token or cloud HSM, per the CA/Browser Forum's June 2023 key-storage rule. |
| **SignPath Foundation** | **Open and free.** OV-level signing for qualifying open-source projects. |

Note what none of them do: remove the SmartScreen warning on day one. Microsoft
[removed instant trust from EV certificates in 2024](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation);
reputation now builds through download volume for OV and EV alike. Signing's real value is
that reputation attaches to the certificate and carries across releases, where an unsigned
binary starts from zero on every version.

## Eligibility

SignPath's [conditions](https://signpath.org/terms.html), against this project:

| Condition | Status |
| --- | --- |
| No malware or unwanted programs | Yes. Reads and rotates local Claude credentials only; no network destination beyond Anthropic's own usage API, no telemetry, no server. |
| OSI-approved licence, no commercial dual-licensing | Yes — MIT, see [LICENSE](../LICENSE). |
| No proprietary components | Yes. Dependencies are listed in `package.json` and `src-tauri/Cargo.toml`. |
| Actively maintained | Yes. |
| Already released in the form to be signed | Yes, since `v0.1.0` — this was the blocking condition. |
| Functionality documented on the download page | Yes — [README](../README.md) plus release notes generated from [CHANGELOG.md](../CHANGELOG.md). |

## Obligations that need work before applying

Certificates issued in the Foundation's name carry stricter terms. These are the ones that
are not satisfied yet:

- [ ] **Multi-factor authentication** enabled on GitHub and on SignPath for everyone with
      access.
- [ ] **Roles declared** — Authors (may modify code), Reviewers (must approve external
      contributions), Approvers (must authorise each signing request). Single-maintainer
      projects still have to state this explicitly.
- [ ] **Every release manually approved** for signing. This fits the existing flow: the
      release workflow already stages a *draft* that a human publishes, so the signing
      approval sits alongside it rather than adding a new gate.
- [ ] **Code signing policy published** on the project homepage — draft below.

Already satisfied:

- [x] **Built from source verifiably.** Every artifact comes from
      [`release.yml`](../.github/workflows/release.yml) on a tag, on GitHub-hosted runners,
      with a public build log. Nothing is built on a developer machine.
- [x] **Binary metadata carries product name and version.** `productName`, `version` and
      `publisher` in `src-tauri/tauri.conf.json` are stamped into the executable, and CI
      fails if the version disagrees with `package.json` or `Cargo.toml`.

## What changes in the build

`release.yml` already has the signing environment variables listed and commented out at the
`tauri-action` step. SignPath does not use those: it signs as a separate CI step that
uploads the built artifact to their service and downloads the signed result, rather than
signing in-process with a local certificate.

So the wiring is a new step after the build and before the release upload, plus
`SIGNPATH_API_TOKEN` in repository secrets. The commented Windows certificate variables
stay unused and should be deleted at that point rather than left to imply otherwise.

---

## Draft: code signing policy

**Do not publish until the application is approved.** Publishing it earlier would claim a
signing arrangement that does not exist. Once approved, this belongs in the README or as
its own page linked from it.

> ### Code signing policy
>
> Free code signing for Windows binaries is provided by [SignPath.io](https://signpath.io/),
> with a certificate issued by the [SignPath Foundation](https://signpath.org/).
>
> **Roles**
>
> - *Authors* — may modify the source: [@yashsoni369](https://github.com/yashsoni369)
> - *Reviewers* — must approve contributions from outside the author list:
>   [@yashsoni369](https://github.com/yashsoni369)
> - *Approvers* — must authorise each signing request:
>   [@yashsoni369](https://github.com/yashsoni369)
>
> **Privacy.** This program does not transfer any information to other networked systems
> unless specifically requested by the user or the person installing or operating it. It
> reads and writes Claude Code credential and configuration files on the local machine, and
> contacts Anthropic's usage endpoint only to read quota for accounts the user has added.
> It collects no telemetry and operates no server. See
> [Before you install this](../README.md#before-you-install-this-what-it-does-with-your-tokens).

## After approval

- Sign only tagged releases; never sign an artifact built outside `release.yml`.
- Keep `SHA256SUMS.txt` regardless. It covers macOS and Linux, which this does not, and it
  stays the check for anyone who does not trust the signature chain.
- Update the README's "These builds are unsigned" section — it will be true for macOS and
  Linux, and false for Windows.
