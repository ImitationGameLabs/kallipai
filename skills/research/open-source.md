---
name: Open-Source Research
description: When researching open-source software or a technical ecosystem — GitHub, metadata, shallow clones, and community signals
---

# Open-Source Research — repos, metadata, community

The domain flow for investigating software, libraries, tools, and
technical ecosystems. GitHub is the primary hub; community signals
(Stack Overflow, Reddit, Hacker News, blogs) reveal adoption, pain
points, and real-world usage that the repo alone does not show.

## When to use

- You are evaluating a library, framework, or tool for adoption
- You are mapping a technical landscape (e.g., "what Rust web frameworks
  exist and how do they compare")
- You need to understand how a project works, its health, and its
  community

## When NOT to use

- The topic is primarily academic (papers, theory) — that is
  `research/academic`.
- You already know the repo and just need to understand its code — that
  is `code/exploring`.

## The sequence

**Search and discover candidates.** Use GitHub's search to find relevant
projects. For a landscape survey, search by topic or keyword:

```bash
# GitHub API search (structured, no browser needed)
curl -s "https://api.github.com/search/repositories?q=rust+web+framework&sort=stars&order=desc&per_page=10"
```

The API returns JSON with name, description, stars, language, topics,
last-updated, and license. For broader discovery, supplement with web
search for "awesome" lists, blog posts, and comparison articles.
Done when:

- you have a candidate list of relevant projects with basic metadata
- you can name the top 3-5 candidates worth deeper investigation

**Gather metadata and assess health.** For each candidate, collect:

- Stars, forks, open/closed issue ratio (activity and adoption signal)
- Last commit date and release cadence (is it maintained?)
- License (compatibility with your use case)
- Open issues and recent PRs (community health, responsiveness)

The GitHub API provides this without cloning or browsing:

```bash
curl -s "https://api.github.com/repos/tokio-rs/axum"
curl -s "https://api.github.com/repos/tokio-rs/axum/releases?per_page=3"
```

Done when:

- you can assess each candidate's health and adoption at a glance

**Shallow-clone and inspect locally.** For projects worth deep
investigation, clone with `--depth=1` — you get the full file tree
without history, fast:

```bash
git clone --depth=1 https://github.com/tokio-rs/axum.git
```

Then follow `code/exploring` to understand the project: root listing →
README → architecture docs → key modules. This is where you learn how
the project actually works, not just what it claims.
Done when:

- you understand the project's architecture and key design decisions
- you can explain what the project does and how

**Assess community signals.** The repo tells you what the project is;
community tells you what it's like to use. Check:

- Stack Overflow: common questions, pain points, gotchas
- Reddit (r/rust, r/programming): adoption sentiment, comparison threads
- Blog posts and tutorials: real-world usage patterns, benchmarks
- GitHub Issues/Discussions: active bugs, feature requests, responsiveness

This step is where `agent-browser` earns its place — these sites need
rendering or interaction. Use it to read discussion threads.
Done when:

- you can describe the community sentiment and common pain points
- you have enough signal to assess real-world adoption

## Key behaviors to remember

- **API over browser for GitHub data** — the GitHub API returns clean
  JSON for search, metadata, and releases; browser automation is
  unnecessary and rate-limited harder, because the API is designed for
  exactly this.
- **Shallow clones, not full clones** — `--depth=1` gives the file tree
  without git history, because research needs the current state, not the
  commit log, and full clones are slow for large repos.
- **Community signals are primary data** — stars and READMEs tell you
  what a project claims; forums and issues tell you what users actually
  experience, because adoption sentiment is a research finding, not
  noise.
- **Browser for forums, API for repos** — GitHub has a first-class API;
  Stack Overflow and Reddit often need rendering, so use `agent-browser`
  selectively for the sources that require it.

## Anti-patterns

- **Deep-cloning repos for research** — `git clone` without `--depth=1`,
  because the full history is noise for understanding the current state
  and costs time and disk for large repos.
- **Skipping community signals** — evaluating a project from its repo
  alone, because real-world pain points and adoption patterns live in
  discussions, not in code.
- **Using agent-browser for GitHub API data** — scraping GitHub's web UI
  when the API returns structured JSON, because the API is faster, more
  reliable, and not subject to rate-limiting or layout changes.
