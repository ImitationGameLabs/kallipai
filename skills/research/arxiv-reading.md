---
name: arXiv Reading
description: How to download and read arXiv papers via their LaTeX source — the lowest-noise format for agent comprehension of academic papers
---

# arXiv Reading — read papers by their source

arXiv papers are written in LaTeX. The `/e-print/` endpoint serves the
original `.tex` source as a compressed tarball — clean, structured text
with zero rendering noise. For an agent, this is dramatically better
than reading the HTML abstract page, the PDF, or extracting text from
either: a typical paper's main `.tex` file is 100-300 lines of readable
markup, versus thousands of tokens of HTML boilerplate or PDF extraction
artifacts.

## When to use

- You need to read the full content of an arXiv paper (not just the
  abstract — that is an API query away)
- You are doing academic research (`research/academic`) and need to read
  selected papers deeply

## When NOT to use

- You only need the abstract, title, or authors — query the arXiv API
  instead (`curl` the API, read `<summary>`).
- The paper is not on arXiv — use `agent-browser` to read the publisher
  page, or find a preprint.

## The download

```bash
# Download the LaTeX source tarball
curl -sL "https://arxiv.org/e-print/2310.08560" -o paper.tar.gz
```

The response is `application/gzip`. Extract it:

```bash
mkdir paper-src && tar xzf paper.tar.gz -C paper-src
```

## Finding the main file

The tarball typically contains one main `.tex` file plus supporting
files. Identify the entry point:

```bash
ls paper-src/*.tex
# main.tex  (most common)
# paper.tex  (also common)
```

The main file is the one with `\documentclass` and `\begin{document}`.
Other `.tex` files are section fragments pulled in via `\input{}` or
`\include{}` — follow that chain to read the full paper.

## What to read, what to skip

**Read:**

- The main `.tex` file(s) — this is the paper. Sections are marked with
  `\section{}`, `\subsection{}`, `\paragraph{}`. The abstract is between
  `\begin{abstract}` and `\end{abstract}`. Tables and math are readable
  as LaTeX markup.
- `.bbl` file — the compiled bibliography. Contains full citation
  entries (authors, title, venue, year). Useful for tracing citations
  without looking up each one.

**Skip:**

- `.sty`, `.bst`, `.cls` files — LaTeX style files, bibliography styles,
  and document classes. They are formatting directives, not content.
- `images/` or any binary files (`.pdf`, `.png`, `.eps`) — figures, not
  readable as text. If a figure's content matters, check the caption in
  the `.tex` file.

## Reading efficiency

A `.tex` file is markup, not prose. Read past the LaTeX commands to the
text content:

- `\textbf{key finding}` → the bold text is the finding
- `\cite{smith2023}` → a citation; look up the key in the `.bbl`
- `$E = mc^2$` → inline math, readable as-is
- `\begin{table}...\end{table}` → table data in tabular format

You do not need to render the paper. The markup preserves all the
semantic structure (sections, emphasis, citations, math) in a form you
can read directly.

## Edge cases

- **Single gzipped file, not a tarball.** Some papers are a single
  `.tex` file compressed with gzip. If `tar xzf` fails, try
  `gunzip paper.tar.gz` — you may get a `.tex` file directly.
- **Main file not named `main.tex`.** Check for `\documentclass` to find
  the entry point: `grep -l documentclass paper-src/*.tex`.
- **arXiv ID formats.** New format: `2310.08560` (YYMM.NNNNN). Old
  format: `cs.AI/0608121` (category/YYMMNNN). Both work with the
  `/e-print/` endpoint.

## *Avoid: reading papers via HTML or PDF

- *Avoid: `agent-browser get text` on the arXiv abstract page for full
  paper content — the abstract page contains only metadata, not the
  full paper, because the full text lives in the e-print source; use the
  API for the abstract, `.tex` source for the full paper.
- *Avoid: downloading and extracting the PDF — PDF text extraction
  introduces artifacts (broken hyphenation, lost structure, figure
  captions interleaved with body text), because the PDF is a rendering
  target, not a source format.
- *Avoid: reading `.sty`/`.bst`/`.cls` files — these are LaTeX
  infrastructure, not paper content, because they contain formatting
  rules with zero information about the research.

## Key behaviors to remember

- **Source over rendering** — `.tex` is the paper's native format; HTML
  and PDF are renderings of it, so reading the source gives you
  everything with less noise.
- **`\documentclass` marks the entry point** — when a tarball has many
  files, the one with `\documentclass` is the main file, because only
  the entry point declares the document class.
- **`.bbl` is the citation lookup table** — the compiled bibliography
  has full citation details keyed by the `\cite{}` labels in the text,
  so you can trace references without leaving the downloaded source.
