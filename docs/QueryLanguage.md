# Query language reference

Flavite's query expressions power `flavite-cli query`, `flavite-cli search`,
and the desktop app's Search view. They parse into a typed query AST
(`flavite_index::Query`, re-exported as `flavite_api::Query`). This document
describes the textual syntax and execution semantics as implemented today in
`crates/flavite-index/src/query.rs`.

## Two input modes (RFC-0006 S06, D-07)

**Expression mode** (explicit operators — `flavite-cli query` and
`flavite-cli search --mode expression`):

```
query     := or-expr
or-expr   := and-expr ( "OR" and-expr )*
and-expr  := unary    ( "AND" unary )*
unary     := "NOT" unary | "(" or-expr ")" | "unresolved" | "tag:" value
           | field-filter | "text:" value | quoted-phrase | bare-term
```

- Operator precedence: **NOT > AND > OR**; keywords are case-insensitive.
- **There is no implicit operator**: adjacent terms without `AND`/`OR` are
  a syntax error (`unexpected token`), not an implicit conjunction.
- `'…'` or `"…"` quotes a **phrase**: the token sequence must match
  positionally through the pinned analyzer.

**Plain full-text mode** (`flavite-cli search` default, the desktop app's
default): whitespace separates `AND`ed terms; quoted runs are one phrase;
punctuation is text — an unknown `key:value` form is searched literally,
never interpreted as a field.

## Terms

| Syntax | Meaning |
|---|---|
| `tag:VALUE` | Match notes with that tag. Value is normalized (lowercased, Unicode-folded): `tag:Rust` matches `rust`. |
| `title:`/`aliases:`/`tags:`/`body:`/`all:` | **Field filter**: restrict the text term/phrase to that indexed field (§6.9). Also available as `search --field title`. |
| `text:VALUE` | Explicit text leaf over all indexed fields. Nested engine syntax (`text:title:rocket`) is rejected with `unsupported legacy text syntax` and the concrete repair (use the field filter). |
| `unresolved` | Notes containing unresolved links. |
| anything else | A bare text term over all indexed fields (in plain mode punctuation is text). |

Any other `key:value` token in **expression mode** fails with
`unsupported query syntax: <token>`. There is no lenient hidden field-query
grammar inside a text leaf.

## Semantics

### Execution routing

- The **structural endpoint** (`flavite-cli query`) translates
  text-free ASTs to SQL with keyset pagination. A text predicate anywhere
  in the AST fails with `text predicates are not supported on the
  structural query endpoint; use the search session endpoint` — never an
  empty success.
- The **search session endpoint** (`flavite-cli search`, the app's Search
  view) captures a coherent snapshot — one SQL read transaction at
  generation G plus a Tantivy searcher stamped exactly at G — evaluates
  the complete AST (negation-normal form over snapshot-local bitsets,
  contributing-leaf masks, per-distinct-leaf BM25 scores) and materializes
  sealed result pages. A session's snapshot is frozen: later saves only
  mark it older; **Refresh** starts a new session explicitly.
- Each distinct positive text leaf in a satisfied branch contributes its
  BM25 score once (a repeated term across `OR` branches counts once);
  negative-only/structural matches score zero and order by native path.
- Ordering: score descending, then canonical native path bytes, then note
  id. Membership is exact at the snapshot's corpus bound; resource limits
  are typed errors, never a truncated "complete" set.

### Full-text engine

- Indexed fields: `title`, `aliases`, `tags`, `body` — field filters
  select the scope; the default searches all of them.
- Matching is case-insensitive and ASCII-folded; CJK text is split into
  single-character unigrams (a multi-token input matches the token
  sequence).
- Scores are engine-internal and relative-only — meaningful within one
  result set, never comparable across queries or index versions.

### Pagination and limits

- Session pages are **fixed-size slices of the materialized spool** (page
  size 1–200, default 50; the desktop app uses 50). Continuation tokens
  bind the session, query digest, snapshot version and ordinal; page
  replay is idempotent and never reruns a query. The end token is absent
  after the last page, and the exact `total` is available after
  materialization.
- `flavite-cli query` fetches `N+1` and reports whether more exist; there
  is **no offset parameter**.
- Excerpts are built from **saved** content around the first matched term;
  unsaved edits are absent by design.

## Examples (verified against the parser)

```sh
flavite-cli search "rocket science"                 # plain mode: whitespace AND
flavite-cli search "quick brown" --mode expression  # error: no implicit AND
flavite-cli search "quick AND brown" --mode expression
flavite-cli search "\"quick brown fox\""           # quoted phrase (both modes)
flavite-cli search "rocket" --field title           # field-filtered term
flavite-cli search "title:rocket AND moon" --mode expression
flavite-cli query "tag:rust"                        # structural tag match
flavite-cli query "tag:foo AND NOT unresolved"      # structural combination
flavite-cli search "pagemark token" --all --json    # consume every page token
```

Queries that fail, as designed:

| Input (mode) | Error |
|---|---|
| `a b` (expression) | `unexpected token` — no implicit AND; write `a AND b` |
| `bogus:foo` (expression) | `unsupported query syntax: bogus:foo` |
| `text:title:rocket` | `unsupported legacy text syntax` — use `--field title` / `title:` |
| `tag:` | empty tag value |
| `text:` | empty text value |
| `"unterminated | `unterminated quoted query value` |
| `(a OR b` | `missing closing parenthesis` |
| *(empty string)* | `query is empty` |
| any text term via `flavite-cli query` | `text predicates are not supported on the structural query endpoint` |

## Not expressible textually (yet)

The AST supports more than the textual grammar: property comparisons
(`Prop` with `==`, `<`, `contains`, … over text/number/bool/date values),
`LinksTo`/`LinkedFrom` graph queries, attachment filters by kind/name, and
hierarchical tags. These are reachable programmatically (builder/AST) but
not yet through the textual syntax — the parser rejects their would-be
syntax instead of guessing.
