# Wiki schema

This is your knowledge wiki: a set of interlinked markdown pages that agents
keep current as you work (Andrej Karpathy's "LLM wiki" pattern). Knowledge is
compiled once and kept up to date, instead of being rediscovered from scratch
on every question. You curate sources and direct the work; agents summarize,
cross-reference, file and keep it consistent.

It lives in ZLO local memory under `wiki/`. Agents read and write it with
their memory tools (`memory_tree`, `memory_read`, `memory_search`,
`memory_write`); you can browse and edit it in Settings → Memory.

## Domain

Everything worth remembering across conversations: the people, projects,
tools and decisions in your work, what you prefer, and what you have learned.
Narrow this section when the wiki grows a clear focus.

## Layout

- `wiki/SCHEMA.md`: this file. Conventions every agent follows.
- `wiki/index.md`: catalog of every page, by section, one line each.
- `wiki/log.md`: append-only record of what changed and why.
- `wiki/sources/`: notes on raw sources (articles, papers, transcripts,
  conversations). Never rewritten after filing, except to fix provenance.
- `wiki/entities/`: one page per person, organization, product, project or
  tool.
- `wiki/concepts/`: one page per idea, technique or topic.
- `wiki/comparisons/`: side-by-side analyses.
- `wiki/queries/`: answers worth keeping, filed after a question.

## Conventions

- File names: lowercase words joined by hyphens (`vector-databases.md`).
- Every page starts with frontmatter:

  ```yaml
  ---
  title: Page title
  type: entity | concept | comparison | query | source
  tags: [from the taxonomy below]
  sources: [wiki/sources/some-source.md]
  updated: YYYY-MM-DD
  confidence: high | medium | low
  ---
  ```

- Link pages with `[[page-name]]`. Each page links to at least two others.
- When a page changes, bump `updated`.
- Every new page gets a line in `index.md` under its section.
- Every change gets a line in `log.md`: `YYYY-MM-DD: what changed (why)`.
- When two sources disagree, keep both claims, mark the page
  `contested: true`, and say which source says what.
- Do not create a page for a passing mention. Create one when a subject
  appears in two sources or is central to one. Split a page past ~200 lines.

## Tag taxonomy

Add a tag here before using it.

- people, organization, project, product, tool
- decision, preference, process, lesson
- research, reference, open-question

## Operations

- **Ingest**: file the source under `sources/`, then update or create the
  entity and concept pages it touches, then `index.md` and `log.md`.
- **Query**: read `index.md`, search, read the relevant pages, answer with
  links. File the answer under `queries/` when it is worth keeping.
- **Lint**: find orphan pages, broken links, stale or contested claims,
  missing index entries; fix them and log the pass.
