---
name: Agent Browser
description: Load this skill before running any agent-browser command — for navigating, interacting with, extracting data from, or testing UIs in a real browser
---

# agent-browser — Browser Automation

Load this skill whenever your task involves **interacting with a web
browser** — navigating pages, clicking, filling forms, reading page
content, taking screenshots, testing a web app, or automating any
browser task.

## Load the core skill (required)

**Before running any agent-browser command**, load its built-in core
guide. This is not optional, because the core skill covers the full
command reference, global flags (like `--cdp` for connecting to an
externally launched Chrome), the snapshot/ref interaction model,
waiting strategies, and troubleshooting via `doctor`. Operating
without it leads to trial-and-error on basics the guide already
answers.

```bash
agent-browser skills get core --full
```

This is **progressive disclosure**: this skill tells you _when_ and
_how_ to use agent-browser; `agent-browser skills get core --full`
gives you the _full workflow guide and command reference_. Read it
every session before your first browser command, because the core
guide is updated independently and details drift.

## When NOT to use agent-browser

Reach for **curl** or a plain HTTP fetch when you only need non-interactive
data extraction — a fixed URL that returns the data you want without
JavaScript rendering or user interaction. Browser automation is
heavyweight: it spins up a real browser process, manages sessions, and
pays for rendering you do not need. If `curl <url>` or an API call gets
the data, use that instead of a browser session.

Similarly, prefer a documented API (REST/GraphQL) over browser automation
when one exists — APIs are faster, more stable, and skip the rendering
overhead entirely.

## Pinning for Focused Work

If your current task involves sustained browser interaction — not just a
single screenshot — pin the core guide so it stays available across
turns:

```bash
agent-browser skills get core --full > /tmp/agent-browser-skill.md
```

Then read the file, and in the next turn pin the result with
`context_pin_last` (label
`skill:agent-browser-core`). When the browser work is done,
`context_unpin skill:agent-browser-core` to free context space.

## Specialized Skills

When the task falls outside normal web pages, load a specialized skill
instead of the core guide:

- `agent-browser skills get electron` — desktop apps (VS Code, Slack,
  Discord, Figma)
- `agent-browser skills get slack` — Slack workspace automation
- `agent-browser skills get dogfood` — systematic exploratory testing
  to find bugs and UX issues

List all available skills with `agent-browser skills list`.

## Semantics to remember

These are the things that commonly trip up agents — keep them in mind
even without the core guide pinned:

- **The core loop is snapshot → ref → act → re-snapshot.** Every
  interaction starts with `agent-browser snapshot -i` (interactive
  elements only), which assigns compact `@eN` refs to each element.
- **Refs are ephemeral.** A ref (`@e3`) is valid only until the page
  changes — after a navigation, a click that re-renders, a form submit.
  Always re-snapshot before your next ref interaction, because a stale
  ref points at the wrong element or nothing.
- **The browser persists across commands.** `agent-browser open <url>`
  starts a session that stays alive until `agent-browser close`. Use
  `close --all` to tear everything down when done.
- **Prefer `-i` (interactive-only) snapshots.** The full tree is verbose;
  `-i` filters to elements you can act on, keeping snapshot output
  ~200-400 tokens instead of parsing raw HTML.
- **Open search results by direct URL.** Instead of navigating to a
  search homepage and filling the search box interactively (3+ steps),
  open the search results URL directly: `agent-browser open
  "https://cn.bing.com/search?q=QUERY"` — one step, same results,
  because the results page is just a URL with a query parameter.
- **Connect to an externally launched browser with `--cdp`.** If
  auto-launch fails (sandbox restrictions, missing SUID wrapper), launch
  a raw browser yourself and connect: `agent-browser --cdp 9222 <command>`
  on every command, because `--cdp` routes agent-browser to your browser's
  DevTools Protocol endpoint instead of starting its own.
