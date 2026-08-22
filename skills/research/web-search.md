---
name: Web Search
description: How to choose and use search engines for research — which engines work via curl vs agent-browser, probing availability, optimized browser workflows, and parsing results with minimal noise
---

# Web Search — engine selection and access strategy

Not all search engines work the same way for an agent. Some return clean
HTML to `curl`; others block non-browser requests and require
`agent-browser`. Some work with one but silently return empty results
with the other. This skill carries the empirically tested decision rules
for picking engines and access methods, so you spend effort on results,
not on fighting captchas.

## When to use

- You need to find pages across the open web (blogs, forums, project
  sites, documentation) that are not available through a structured API
  (GitHub, arXiv have their own APIs; use those instead)
- You are in a research flow (`research/open-source`,
  `research/academic`) and need to discover sources beyond structured APIs

## When NOT to use

- GitHub data: use the GitHub API (`research/open-source`)
- arXiv papers: use the arXiv API (`research/academic`)
- You already have the URL: `curl` or `agent-browser` the page directly

## Engine index

Each engine has been tested with both `curl` (browser User-Agent) and
`agent-browser` (headless Chromium). The access behavior is empirical —
always probe before relying on an engine in a new environment, because
bot detection and regional restrictions change.

### Bing (cn.bing.com) — works with both curl and agent-browser

The most reliable engine for agent access. Returns full server-side
HTML, so `curl` works directly. `agent-browser` also works, with cleaner
structured output.

**curl approach:**

```bash
curl -sL -A "Mozilla/5.0" "https://cn.bing.com/search?q=rust+web+framework" \
  | grep -oP 'href="(https?://[^"]*)"' \
  | grep -v 'bing\.com\|microsoft\|msn\.com\|miit\.gov\|mps\.gov'
```

Note: `www.bing.com` 302-redirects to `cn.bing.com` in China; always use
`-L` to follow. Results from `cn.bing.com` are biased toward Chinese
sources (zhihu, juejin, CSDN); English sources appear but rank lower.
Filter the government registration links (miit/mps) from the URL list.

**agent-browser approach:**

```bash
agent-browser open "https://cn.bing.com/search?q=rust+web+framework"
agent-browser wait --load networkidle
agent-browser snapshot -i -c -u
```

Three steps. Results appear as level-2 headings paired with links, each
with clean `@eN` refs. Use `-c` (compact) to drop empty structural nodes
and `-u` (urls) to include hrefs inline. Extract URLs either by parsing
the snapshot text or by `get attr @eN href` on individual result links.

### DuckDuckGo — degrades in both modes

**curl:** the HTML endpoint (`html.duckduckgo.com/html/`) serves a
"botnet" anomaly page — a full captcha wall, not results.

**agent-browser:** the page loads and the URL/title are correct, but the
results area renders **empty**. DuckDuckGo's headless detection is
silent: no error, no captcha, just no results in the DOM. This is a
trap — an agent that does not verify result count will assume the query
returned nothing.

Do not rely on DuckDuckGo for agent search. If you must try it, verify
that the snapshot contains result links (not just the search box) before
proceeding.

### Google / Google Scholar — IP-level blocking

Both curl and agent-browser get blocked. Google returns a `/sorry/`
redirect to a captcha page; Scholar returns "unusual traffic from your
computer network." This is **IP-level blocking**, not headless detection
— agent-browser cannot bypass it without a proxy. If the operator's
environment has a proxy, Google becomes viable; otherwise treat it as
unreachable.

### Baidu — noisy for technical queries

Returns a large page via curl (888KB+), but results are mixed with
hot-search trending topics, and the result links are wrapped in Baidu
redirect URLs that require an extra extraction step. `agent-browser`
snapshot gives cleaner structure but still carries significant noise.
Use as a supplementary source only, for China-specific content.

## Probing engine availability

At the start of a research session, probe which engines are reachable.
This takes seconds and prevents wasted effort on engines that will fail
silently:

```bash
# curl probe: does the engine respond with real content?
curl -sL -m 5 -A "Mozilla/5.0" "https://cn.bing.com/search?q=test" | wc -c
# >50000 bytes = likely real results; <500 = blocked or redirect

# Google probe: check if we get a captcha page
curl -sL -m 5 -A "Mozilla/5.0" "https://www.google.com/search?q=test" | grep -c "sorry"
# 0 = available, >0 = blocked
```

For browser-required engines, probe with agent-browser and **verify
result count**:

```bash
agent-browser open "https://cn.bing.com/search?q=test"
agent-browser snapshot -i -c
# Verify: count lines with "heading" and "level=2" — should be >3 for a real results page
```

## Engine priority and fallback

The priority order depends on environment (region, proxy availability):

**Without proxy (China environment):**

1. **Bing** (curl or agent-browser) — primary. Works reliably, returns
   real results.
2. **Baidu** — supplementary only, for China-specific queries.

**With proxy:**

1. **Google** (agent-browser) — broadest index.
2. **Bing** — supplementary.
3. **Baidu** — supplementary, for China-specific content.

**Always avoid as primary:** DuckDuckGo (silent empty results in
agent-browser, captcha wall for curl).

Probe first, then commit to the order for the session.

## Choosing curl vs agent-browser

For Bing (the reliable engine), both methods return the same result URLs.
Choose by what you need:

- **Use curl** for quick URL extraction — fastest, no browser overhead,
  parallelizable across many queries. Filter noise links with `grep -v`.
- **Use agent-browser** when you need to click into results, interact
  with the search page (filters, pagination), or when curl returns
  degraded HTML. The snapshot gives structured title+URL pairs that are
  easier to triage by reading.

The practical default: **curl for the initial search, agent-browser to
visit and read the result pages**. This is because curl extracts URLs in
one command while agent-browser excels at rendering and reading the
target pages (many of which are JS-heavy or need interaction).

## Reading result pages

Search results give you URLs; reading the pages at those URLs is the next
step. Most result pages (blogs, docs, project sites) are JS-heavy or
require rendering — this is where agent-browser earns its place:

```bash
agent-browser open "https://rocket.rs/"
agent-browser wait --load networkidle
agent-browser get text body    # full page text for reading
# or for targeted extraction:
agent-browser snapshot -i -c   # interactive structure
```

For static pages (simple HTML), `curl` + reading is fine. For
JS-rendered pages (React/Next.js sites, SPAs), `agent-browser` is
required because curl gets an empty shell.

## Multi-engine strategy

For broad research, querying multiple engines increases coverage:

- Different engines index different content and rank differently
- Regional engines surface local sources invisible to global engines
- Run queries in parallel (multiple subagents or background tasks) when
  speed matters, then merge and deduplicate results

## *Avoid: approaches that waste effort

- *Avoid: curl-ing DuckDuckGo — it serves a "botnet" anomaly captcha
  page to non-browser clients, because its bot detection flags curl
  traffic.
- *Avoid: relying on DuckDuckGo in agent-browser without verifying
  results — it returns a **silently empty** results page in headless
  mode, because its headless detection suppresses results without error;
  an agent that does not check result count will misread silence as "no
  results found."
- *Avoid: expecting agent-browser to bypass Google/Scholar IP blocks —
  the block is at the network level, not the browser level, so a
  headless browser hits the same captcha wall as curl.
- *Avoid: relying on a single engine — coverage gaps are real,
  especially across regions, because no engine indexes everything.
- *Avoid: deep-reading search result pages — extract URLs and triage by
  title/snippet first, because a search page is an index, not a source.

## Key behaviors to remember

- **Probe before committing** — spend 5 seconds checking engine
  availability at session start, because bot detection and regional
  blocks change without notice.
- **Verify result count, not just page load** — some engines (DuckDuckGo)
  return a valid-looking page with zero results, because silent failures
  look identical to "no results" if you do not check.
- **curl for search, browser for reading** — curl extracts search URLs
  in one command; agent-browser renders and reads the JS-heavy result
  pages that curl cannot, because each tool's strength maps to a
  different step in the workflow.
- **Bing over Baidu as primary** — even in China, Bing tends to return
  cleaner technical results, because Baidu's results are noisier for
  non-Chinese technical queries.
- **Merge across engines for coverage** — each engine has blind spots,
  so querying 2-3 and deduplicating catches sources a single engine
  misses.
