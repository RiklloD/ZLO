---
name: llm-wiki
version: 1.0.0
description: Keep the user's knowledge wiki in ZLO memory (Karpathy's LLM wiki) - ingest sources, answer from it, lint it.
activation:
  keywords:
    - wiki
    - knowledge base
    - ingest
    - file this
    - add to my notes
    - what do I know about
    - lint the wiki
    - remember this source
  patterns:
    - "(?i)(add|save|file|ingest) (this|that|it) (to|in|into) (the |my )?(wiki|knowledge base|notes)"
    - "(?i)(update|lint|audit|check) (the |my )?wiki"
    - "(?i)what does (the |my )?wiki say"
  tags:
    - memory
    - knowledge-base
    - wiki
  max_context_tokens: 2200
---

# Knowledge wiki (LLM wiki)

Ported from Hermes' `llm-wiki` skill (MIT), after Andrej Karpathy's "LLM
wiki" pattern. The user's knowledge lives as interlinked markdown pages that
you keep current, so it is compiled once instead of rediscovered every time.
The user curates sources and asks questions; you summarize, cross-reference,
file and keep it consistent.

## Where it lives

In ZLO local memory under `wiki/` (not on disk). Use your memory tools:

- CLI agents: `memory_tree`, `memory_read`, `memory_search`, `memory_write`.
- ZLO's own agent: the `ironclaw.memory.*` tools.

`memory_write` replaces a whole document. To change an existing page, read it
first and pass its `content_hash`; without the hash the page must not exist
yet. Never write under `archive/`.

If `wiki/SCHEMA.md` is missing, the user has not set the wiki up: say so and
point them to Settings → Memory → "Build the wiki" (or create the three
files yourself if they ask).

## Orient first, every session

1. Read `wiki/SCHEMA.md` (conventions, tag taxonomy, layout).
2. Read `wiki/index.md` (every page, one line each).
3. Read the end of `wiki/log.md` (recent changes).

Then search for the topic before creating anything, so you extend pages
instead of duplicating them.

## Ingest (a URL, file, paste or conversation worth keeping)

1. File the source as `wiki/sources/<descriptive-name>.md` with frontmatter
   (`source_url`, `ingested: YYYY-MM-DD`). Sources are not rewritten later.
2. Tell the user the two or three takeaways that matter for them (skip in
   automated runs).
3. Search the wiki for every entity and concept the source touches.
4. Update existing pages (bump `updated`, keep both claims with dates when
   sources disagree and mark `contested: true`); create pages only for
   subjects central to the source or seen in two sources.
5. Every new or changed page links to at least two others with `[[name]]`.
6. Add new pages to `wiki/index.md`; append one line per change to
   `wiki/log.md`: `YYYY-MM-DD: what changed (why)`.
7. List every page you created or changed.

One source often touches 5-15 pages; that is the point. Ask first before an
ingest that would change more than ten existing pages.

## Query

Read the index, search, read the relevant pages, answer citing them
(`[[page]]`). When the answer took real synthesis (a comparison, a deep dive),
file it under `wiki/queries/` or `wiki/comparisons/`, index it and log it.

## Lint (when asked to check or tidy the wiki)

Find and fix: pages missing from the index, broken `[[links]]`, orphan pages
nobody links to, pages without frontmatter or with tags outside the
taxonomy, pages over ~200 lines (split them), and `contested` or
`confidence: low` pages to raise with the user. Log the pass with the number
of issues found.

## Rules

- Never invent facts to fill a page; say what is unknown.
- Keep pages readable in 30 seconds; move detail into linked pages.
- Add a tag to the taxonomy in `SCHEMA.md` before using it.
- Never drop a claim silently: supersede it with a dated note.
