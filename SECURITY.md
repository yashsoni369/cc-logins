# Security policy

CC Logins reads, refreshes, and writes Claude Code OAuth credentials. A bug here
can expose an authentication token, so security reports are taken seriously and
handled privately.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.** A public report
against a credential-handling app is a working exploit against everyone running
it, published before there is a fix.

Report it through GitHub's private reporting instead:

- **[Open a private security advisory](https://github.com/yashsoni369/cc-logins/security/advisories/new)**

Include what you would put in any bug report — OS, app version, steps — plus
what you believe the impact is. A proof of concept helps, but a clear
description of the mechanism is worth more than a script.

This is a single-maintainer, pre-1.0 project. There is no guaranteed response
time and it would be dishonest to publish one. Reports are read and answered as
soon as they are seen, and you will get an acknowledgement before any fix.

**Never include a real token, credential file, or the contents of
`.credentials.json` in a report.** Redact them. If a bug can only be shown with
real credential material, say so and we will work out how to reproduce it
without sending any.

## Supported versions

Only the latest release. At `0.x` there are no maintenance branches, and
backporting a fix to a version nobody is running would be theatre.

## What this app does with your tokens

Stated here as well as in the README, because it defines what is and is not a
vulnerability in this project:

- Credentials are read from and written to Claude Code's own credentials file,
  and kept in this app's own vault under its app-data directory.
- On Windows the stored copy is encrypted with DPAPI; on macOS it goes in the
  Keychain; **on Linux it is a `0600` file and is not encrypted.** That last one
  is a known gap, is recorded in the stored envelope's `scheme` field rather
  than being papered over, and is not a bug report — though a way to close it
  is very welcome.
- Nothing is sent anywhere except Anthropic's own OAuth and usage endpoints.
  There is no server, no telemetry, and no cloud sync. Any observed network
  traffic to a third party **is** a vulnerability, and an urgent one.

## Builds are unsigned

Releases are not code-signed, so Windows SmartScreen and macOS Gatekeeper will
warn. That is expected and documented in the README — it is not a
vulnerability report, but it does mean **you should verify what you are running
before trusting it with your tokens**. Building from source is the strongest
check available today.
