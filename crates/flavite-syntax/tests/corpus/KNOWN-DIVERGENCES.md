# KNOWN-DIVERGENCES

Corpus fixtures where the tree-sitter path (`parse_note_ts`) deliberately
diverges from the pulldown path (`parse_note`), with a written reason for
each. This file is the acceptance record for RFC-0008 AC-5 waivers.

Protocol: every fixture listed here MUST also appear in `WAIVED` in
`tests/parity.rs`; the parity test fails if the two lists drift apart.
Fixing mask construction is preferred over waiving — a waiver is the last
resort after the grammar's capabilities are exhausted.

## Waivers

- `inline_link_anchor.md` — pulldown's scan windows **leak** inline link
  destinations and titles into the prose window when text follows them in the
  same paragraph (the glue's pending-window gap arithmetic counts every byte
  between text events, including `[label](url "title")` internals). The
  tree-sitter path masks `link_destination` / `link_title` (inline grammar),
  so it never scans URLs or titles — the intended semantics, and the safer
  one (no spurious `#anchor` tags or `[[...]]` links from URLs). Byte-parity
  is impossible for this class without reproducing the gap arithmetic, so it
  is accepted and documented instead. The common case (links at paragraph
  ends, URLs without `#`/`^`/`[[` tokens) is proven equivalent by
  `inline_link_clean.md`.

## Appendix — design notes

The block grammar's `minus_metadata` node and the shared
`mask::find_frontmatter_close` heuristic keep YAML frontmatter excluded in
both paths. Inline-grammar exclusions (`code_span`, `html_tag`, `latex_block`,
`link_destination`, `link_title`) mirror the pulldown event stream, which
never emits destinations/titles as text.
