---
title: "flavite-editor - Query language reference"
weight: 25
type: docs
draft: false
lastmod: 2026-11-04T21:58:52+00:00
---

# Query language reference

Flavite's query expressions power `flavite-cli query`, `flavite-cli search`,
and the desktop app's Search view. They parse into a typed query AST
(`flavite_index::Query`, re-exported as `flavite_api::Query`). This document
describes the textual syntax and execution semantics as implemented today in
`crates/flavite-index/src/query.rs`.

## Quick reference

```
query     := or-expr
or-expr   := and-expr ( "OR" and-expr )*
and-expr  := unary    ( "AND" unary )*
unary     := "NOT" unary | "(" or-expr ")" | "unresolved" | "tag:" value | "text:" value | bare-term
```

- Operator precedence: **NOT > AND > OR**.
- Operator keywords (`AND`, `OR`, `NOT`, `unresolved`) are
  case-insensitive (`and`, `Or`, `not` all work).
- Tokens are separated by whitespace; `(` and `)` are always standalone.
- `'…'` or `"…"` quotes a value containing spaces or parentheses. The same
  quote character must close it.
- **There is no implicit operator**: adjacent terms without `AND`/`OR` are
  a syntax error (`unexpected token`), not an implicit conjunction.

## Terms

| Syntax | Meaning |
|---|---|
| `tag:VALUE` | Match notes with that tag. Value is normalized (lowercased, Unicode-folded): `tag:Rust` matches `rust`. Hierarchical semantics (`a/b` matching `a/b/c`) exist in the AST but **not** in the textual syntax — `tag:a/b` matches the literal tag `a/b`. |
| `text:VALUE` | Full-text search of the note body/title/tags over VALUE. |
| `unresolved` | Notes containing unresolved links. |
| anything else | A bare full-text term (same as `text:<term>`). |

Everything else is deliberately rejected rather than misinterpreted: any
other `key:value` token (e.g. `title:foo`, `links-to:x`) fails with
`unsupported query syntax: <token>`.

## Semantics

### Execution routing

- A pure text query goes to the full-text engine.
- An `AND` of text and structural parts runs the text parts jointly through
  the full-text engine and applies the structural part as a filter.
- A structural-only query (`tag:…`, `unresolved`, and combinations of them)
  runs directly against SQLite, ordered by note id, with no relevance score.

### Full-text engine

- Indexed fields: `title`, `aliases`, `tags`, `body` — searched jointly.
- Matching is case-insensitive and ASCII-folded; CJK text is split into
  single-character unigrams.
- The text value is itself parsed by the search engine's own (lenient)
  field-query syntax, so **field-scoped search works inside text values**:
  `text:title:rocket` restricts to the title field. Invalid parts are
  silently dropped (lenient parsing) rather than erroring.
- Scores are engine-internal and relative-only — meaningful within one
  result set, never comparable across queries or index versions.

### Pagination and limits

- `--limit N` caps the result count. The CLI fetches `N+1` and reports
  whether more exist; there is **no offset parameter**.
- Defaults: `query` limit 50, `search` limit 20; the desktop app's search
  view clamps its limit to 1–200 and treats whitespace-separated input
  words as `AND`ed terms, parsing each structured term (e.g. `tag:…`)
  individually.
- Excerpts are built from **saved** content around the first matched term;
  unsaved edits are absent by design.

## Examples (verified against the parser)

```sh
flavite-cli query "rust"                            # bare full-text term
flavite-cli query "tag:rust"                        # structural tag match
flavite-cli query "tag:foo AND text:'y'"            # tag + quoted text value
flavite-cli query "(a OR b) AND NOT c"              # grouping and negation
flavite-cli query "tag:rust AND NOT unresolved"     # combination
flavite-cli query "NOT (tag:x OR \"hello world\")"  # quoted phrase inside NOT
flavite-cli query "tag:x AND tag:y"                 # conjunction of tags
flavite-cli query "unresolved"                      # all notes with unresolved links
```

Queries that fail, as designed:

| Input | Error |
|---|---|
| `a b` | `unexpected token` — no implicit AND; write `a AND b` |
| `title:foo` | `unsupported query syntax: title:foo` (use `text:title:foo`) |
| `tag:` | empty tag value |
| `text:` | empty text value |
| `"unterminated | `unterminated quoted query value` |
| `(a OR b` | `missing closing parenthesis` |
| *(empty string)* | `query is empty` |

## Not expressible textually (yet)

The AST supports more than the textual grammar: property comparisons
(`Prop` with `==`, `<`, `contains`, … over text/number/bool/date values),
`LinksTo`/`LinkedFrom` graph queries, attachment filters by kind/name, and
hierarchical tags. These are reachable programmatically (builder/AST) but
not yet through the textual syntax — the parser rejects their would-be
syntax instead of guessing.
