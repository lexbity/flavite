# KNOWN-DIVERGENCES

RFC-0011 S6 retired the tree-sitter engine and its flag: the segment cache
(`SegmentIndex`, RFC-0011 S5) and the whole-document bridge
(`pulldown_glue::parse_normalized`) are the **same** pulldown producer, so
the engine-era waiver protocol no longer applies. The corpus goldens are
generated from the whole-document bridge and the segment cache must equal
them byte-for-byte (RFC-0011 AC-5) — the parity tests in `parity.rs` and
`segment_parity.rs` enforce this.

## Retired waiver (history)

- `inline_link_anchor.md` — waived under RFC-0008 AC-5: pulldown's scan
  windows leaked inline link destinations/titles into prose when text
  followed them in the same paragraph (gap arithmetic), while the
  tree-sitter path masked `link_destination`/`link_title`. With one
  producer there is no second grammar to compare against, so the waiver is
  retired. The fixture remains in the corpus as a regression fixture for the
  bridge's own gap arithmetic.

## Protocol (remaining)

Any future divergence between the whole-document bridge and the segment
cache must be fixed or explicitly waived here AND in both parity tests; the
tests fail if the lists drift apart.
