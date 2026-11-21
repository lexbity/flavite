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

## Waived: C0 control characters

Pulldown treats several C0 control characters (e.g. `\x0f`, `\x0b`,
`\x1d`, `\x1a`) as line terminators or parse aborters in ways the segment
scanner's line model does not model (it splits on `\n` only, after CRLF
folding). Such characters are not part of the supported document grammar;
the `parity` fuzz target skips inputs containing them, as it does
standalone-CR and NUL-containing input.

## Waived: container-context fences after list markers

Pulldown does not open a fenced code block immediately after a list-item
marker line (the fence line is absorbed into the item's paragraph) unless
the item itself has ended; the segment scanner's line model has no list
container state. Inputs where a fence-shaped line follows a list-marker
line directly are excluded from the `parity` fuzz target. The blank-free
list-fence shape remains covered by the corpus goldens
(`fence_in_list.md`) and the unit tests, where both paths agree.

## Waived: HTML constructs in the differential fuzz target

Pulldown's HTML state (block types 1–7 with attributes, comments, CDATA,
and container interactions) can span segment boundaries; the segment
scanner's HTML model is deliberately coarse. Inputs containing `<` are
excluded from the `parity` fuzz target; HTML handling is covered by the
corpus goldens and unit tests.

## Waived: inline-extension delimiters (`$` math, `~~` strikethrough)

The bridge masks `$...$` math and `~~...~~` strikethrough spans, which can
stretch across the segment scanner's boundaries (the scanner models
fences/frontmatter/HTML only, not inline-extension state). Inputs
containing `$` or `~~` are excluded from the `parity` fuzz target; the
masking itself remains covered by the corpus goldens where the spans are
segment-local.

## Waived: standalone `\r` line endings

The segment scanner's line model (`StructureText`) splits on `\n` only
(CRLF is folded to LF during normalization, §4.8 R12). The pulldown bridge
follows CommonMark and additionally treats a standalone `\r` as a line
ending. For a document using bare-CR line breaks, the two paths therefore
disagree on where fences begin/end. This is a documented line-model
limitation, not a classification regression: the editor's normalization
contract (R12) folds CRLF and leaves standalone CR untouched, and such
documents are outside the parity harness's supported input universe (the
`parity` fuzz target skips them, as it does NUL-containing or
undecodable input).

## Protocol (remaining)

Any future divergence between the whole-document bridge and the segment
cache must be fixed or explicitly waived here AND in both parity tests; the
tests fail if the lists drift apart.
