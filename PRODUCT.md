# Product

## Register

product

## Platform

web

## Users

Developers who hold more than one Claude subscription and run Claude Code all day. They are mid-task
when they touch this — a build is running, a thought is half-finished — so the interaction budget is
a glance and one click. The job to be done is *never lose flow to a rate limit*: know how much
headroom is left before it bites, and move to another account without leaving the keyboard.

Secondary context worth designing for: the same person on a second machine, and on Windows the same
person split across a native Windows install and a WSL distro, where Claude Code keeps two entirely
separate logins.

## Product Purpose

An always-present tray application that shows which Claude account is active, how much 5-hour and
7-day quota each account has left, and switches between them — manually in one click, or
automatically before a limit lands. It also keeps a local history of usage so burn rate is visible
over days and weeks, which no existing tool offers.

Success is that the user stops thinking about accounts. They never hit a limit unexpectedly, and
they never open a terminal to find out where they stand.

## Positioning

The only account switcher that never touches your tokens except to hand them to the official Claude
Code binary — no relay, no proxy, no cloud sync, no export button.

## Brand Personality

An instrument, not an app. Precise, quiet, and legible at a glance. It reports; it does not advise,
celebrate, or chat. Voice is terse and factual — a gauge reads "61%", not "You're doing great!".
Three words: **precise, calm, honest.**

Honest is load-bearing. This software reads authentication tokens, so it states plainly what it
does, what it does not do, and where it stands on Anthropic's terms. It never overstates freshness
of data — stale numbers are labelled stale.

## Anti-references

- **Anthropic's own coral-and-cream identity.** Mimicking it would imply this is official. It is
  not, and for a tool that reads auth tokens that confusion is a trust failure, not a style choice.
- **The generic SaaS dashboard.** Stat-card grids, gradient accents, rounded-everything, a chart
  present because dashboards have charts.
- **Gamer / neon utility aesthetics.** Saturated glows, RGB accents, dark-with-cyan. The lane most
  developer tray tools default into.
- **Enterprise admin panels.** Grey chrome, stacked toolbars, tiny system type, joyless density.

## Design Principles

**Color is state, never decoration.** At rest the interface is monochrome — bone markings on a
near-black instrument face. Color appears only as an account approaches a limit. A screen with no
color on it means nothing needs the user; any color at all means look.

**Glanceable before readable.** The primary surface is a tray icon a few pixels wide. Every design
decision is tested against "can this be understood in under a second, from the corner of the eye."
Detail is available on demand, never required.

**Numbers are the subject.** Quota, percentages, countdowns and reset times are the content. They
get monospace, tabular figures, and the strongest position in every layout. Chrome recedes.

**Never lie about freshness.** Usage data ages, WSL distros sleep, networks fail. Every number
carries its age when it is not current, and the interface degrades to labelled-stale rather than
blank or silently wrong.

**Earned familiarity.** Standard affordances, consistent component vocabulary, no invented controls.
The tool should disappear into the task.

## Accessibility & Inclusion

WCAG AA as the floor: body text ≥4.5:1, large text ≥3:1. Because the design encodes meaning in
color, **state is never carried by hue alone** — every quota state is also carried by numeric value,
bar fill, and an explicit label, so the interface is fully usable with any form of color blindness.
Full keyboard operation with visible focus states, since the audience lives on the keyboard.
`prefers-reduced-motion` is honoured throughout; the only motion in the product conveys state
change.
