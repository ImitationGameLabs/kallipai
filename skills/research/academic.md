---
name: Academic Research
description: When researching a scientific or academic topic — using arXiv, CrossRef, and Google Scholar, with LaTeX source reading and citation tracing
---

# Academic Research — papers, preprints, citations

The domain flow for academic and scientific questions. Three search
channels, each with a different access method: **arXiv API** for recent
CS/AI/ML/Physics/Math preprints, **CrossRef API** for published papers
with citation counts across all publishers, and **Google Scholar** (via
agent-browser) for the broadest coverage including citation graphs.
Reading papers via their LaTeX source
(`research/arxiv-reading`) is the core efficiency move.

## When to use

- You are researching a scientific topic that has arXiv preprints or
  academic literature
- You need to understand the state of the art, compare approaches, or
  trace an idea through its citations

## When NOT to use

- The topic is better served by code, repos, or community discussion —
  that is `research/open-source`.
- You need a single paper's abstract — `curl` the arXiv API and read
  the summary field; no need for the full flow.

## The sequence

**Search across academic sources.** Different sources index different
papers; use the right one for the query:

**arXiv API** (preprints — CS, AI, ML, Physics, Math). Returns clean
Atom XML via `curl`. No key, no browser:

```bash
curl -s "http://export.arxiv.org/api/query?search_query=all:memory+augmented&max_results=10"
```

`<entry>` elements contain `<title>`, `<summary>` (abstract), `<author>`,
`<link>` (abs page, PDF), and `<arxiv:comment>` (venue, page count).
Filter by category with `cat:cs.AI`.

**CrossRef API** (published papers — all publishers, all fields).
Returns clean JSON via `curl`. No key required. Includes citation
counts, which arXiv does not:

```bash
curl -s "https://api.crossref.org/works?query=memory+augmented+language+models&rows=10&select=title,is-referenced-by-count,DOI,author,abstract,published"
```

Each item has `title`, `author`, `is-referenced-by-count` (citation
count), `DOI`, and `published` date. Use `select` to minimize payload.

**Google Scholar** (broadest coverage + citation graphs). Requires
`agent-browser` — Scholar serves a reCAPTCHA to `curl`, blocking
non-browser access:

```bash
agent-browser open "https://scholar.google.com/scholar?q=memory+augmented+language+models"
agent-browser snapshot -i
```

Scholar's unique value is **"cited by" links** — follow them to find
newer work that builds on a paper, and to gauge impact. This citation
graph is not available via any API.

**Semantic Scholar API** and **OpenAlex API** are structured
alternatives to Scholar with citation data, but both are aggressively
rate-limited for unauthenticated use. Probe them; use them when
available, fall back to Scholar via browser when not.

Done when:

- you have a ranked list of relevant papers with abstracts and citation
  counts
- you have enough candidates to start reading (typically 5-15)

**Select papers to read deeply.** Read abstracts and triage. Prioritize:

- highly cited work (use CrossRef or Scholar citation counts)
- recent survey papers (they map the landscape for you)
- papers directly matching your framed question

Done when:

- you have 2-5 papers selected for deep reading

**Read papers via LaTeX source.** Load `research/arxiv-reading` and
download the `.tex` source. A typical paper's `main.tex` is 100-300
lines of clean text, versus thousands of tokens of HTML noise or PDF
extraction artifacts. Read the abstract, introduction, method, and
results sections; skim related work unless you are mapping the field.
For papers not on arXiv, use CrossRef to get the DOI, then check if an
arXiv preprint exists (many published papers have one).
Done when:

- you understand each paper's contribution, method, and results
- you can explain how each paper relates to your framed question

**Trace citations.** Key papers cite other key papers. Use the
citation tools available:

- **CrossRef `/works/{DOI}/` endpoint** returns citation data via curl
- **Google Scholar "cited by"** finds newer work building on a paper
  (browser-only)
- **arXiv `.bbl` file** lists references within a paper's source

Note recurring citations — papers cited by multiple of your sources are
likely foundational. This is how you find the roots of a field.
Done when:

- you have followed the most important citation chains
- you can name the foundational papers in the area

## Key behaviors to remember

- **API over browser for search** — arXiv and CrossRef return clean
  XML/JSON via curl; browser automation is heavyweight and unnecessary
  for structured search, because the APIs exist specifically for this.
  Reserve Google Scholar (browser) for citation graph traversal, which
  the APIs do not provide.
- **LaTeX source over HTML/PDF for reading** — `.tex` is the
  lowest-noise representation of a paper; load `research/arxiv-reading`
  for the download and parsing workflow.
- **CrossRef gives citation counts arXiv lacks** — use CrossRef when
  you need to assess impact or find highly cited work, because arXiv
  has no citation tracking.
- **Abstracts first, deep-read selectively** — reading every paper fully
  wastes context; triage by abstract and citation count, read deeply
  only the ones that matter for your question.
- **Follow citation chains** — the most important papers are often found
  through the references of papers you already found, because citation
  networks encode the structure of a field.

## Anti-patterns

- **Using agent-browser to search arXiv** — spinning up a browser for a
  task the API handles, because the API returns structured data with
  zero rendering overhead.
- **Reading papers via HTML** — extracting text from the arXiv abstract
  page or PDF, because LaTeX source is cleaner, smaller, and preserves
  structure (sections, math, tables) as readable markup.
- **Reading every paper fully** — deep-reading all search results,
  because context is bounded and most papers are irrelevant after the
  abstract.
- **Using Google Scholar for what CrossRef does** — Scholar requires a
  browser and is rate-limited by captcha; CrossRef returns the same
  citation counts via curl, so use it for citation lookups and reserve
  Scholar for citation-graph traversal.
