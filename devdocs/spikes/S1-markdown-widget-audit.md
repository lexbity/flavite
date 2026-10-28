# Spike S1-2: Markdown widget coverage audit

**Status:** closed. **Slice:** PLAN-A A6 (§9 Phase 0, spike S1, part 2 of 2).
**Date:** 2026-10-26. **Pinned version:** iced 0.14.0.

## Question

What does iced's markdown widget (`iced::widget::Markdown`) support, and
does it cover the features required for Flavite's read mode (§4.6)? The
Renderer trait (§4.6) sits behind read mode, and the decision is whether to
extend iced's widget or replace it.

## Fixture document

To test coverage, we rendered the following fixture document through
`iced::widget::Markdown` at the pinned version (0.14.0):

```markdown
# Heading 1

## Heading 2

### Heading 3

Regular paragraph with *emphasis*, **strong**, `code span`.

- List item 1
- List item 2
  - Nested list item

1. Ordered item 1
2. Ordered item 2

> Blockquote

`code block`

```rust
fn main() {
    println!("code fence");
}
```

| Table | Header |
|-------|--------|
| cell 1 | cell 2 |

- [x] Task list item (checked)
- [ ] Task list item (unchecked)

Footnote[^1]

[^1]: Footnote definition

Inline math: $E = mc^2$

Block math:

$$\int f(x) dx$$

Wikilink: [[note]]

Embed: ![[image.png]]
```

## Findings

### F1 — iced 0.14.0 markdown widget coverage

| Feature | Supported | Notes |
|---|---|---|
| Headings (#, ##, ###) | ✓ | Renders correctly |
| Paragraphs | ✓ | Renders correctly |
| Emphasis (*) | ✓ | Renders as italic |
| Strong (**) | ✓ | Renders as bold |
| Code spans (`` ` ``) | ✓ | Renders with monospace font |
| Unordered lists (-) | ✓ | Renders with bullet points |
| Nested lists | ✓ | Indents correctly |
| Ordered lists (1.) | ✓ | Renders with numbers |
| Blockquotes (> ) | ✓ | Renders with indentation and border |
| Code blocks (```) | ✓ | Renders with background and monospace |
| Tables (|---|) | ✗ | **Not supported** — renders as raw text with pipes |
| Task lists (- [x]) | ✗ | **Not supported** — renders as raw text |
| Footnotes | ✗ | **Not supported** — renders as raw text |
| Math ($...) | ✗ | **Not supported** — renders as raw text |
| Wikilinks ([[...]]) | ✗ | **Not supported** — renders as raw text |
| Embeds (![...]) | ✗ | **Not supported** — renders as raw text |

### F2 — pulldown-cmark under the hood

`iced::widget::Markdown` uses `pulldown-cmark` internally (the same parser
Flavite uses for indexing, per §4.3). pulldown-cmark supports CommonMark
plus extensions (tables, footnotes, task lists are behind cargo features that
iced may or may not enable). Even if pulldown-cmark can parse tables, iced's
widget does not render them.

### F3 — Missing features for Flavite

The following CommonMark / extension features that Flavite needs in read mode
are **not rendered** by iced's markdown widget:

1. **Tables** — the spec does not require table support explicitly, but they
   are part of the interoperability posture (§0: "inspired by Obsidian").
   Obsidian renders tables.
2. **Task lists** — checkboxes are part of Flavite's feature set (D7: conflict
   state includes checkboxes; §4.6: read mode must render them).
3. **Wikilinks** — `[[note]]` is core to Flavite (D8, D10). The widget renders
   it as literal text, not as a link.
4. **Embeds** — `![[image.png]]` is core to Flavite (D8, R3). The widget renders
   it as literal text.
5. **Math** — not a v1 requirement (§4.10: "Math spans MUST be parsed, excluded
   from link and tag extraction, and shown as styled source").
6. **Footnotes** — not a v1 requirement.

### F4 — The Renderer trait decision

§4.6 explicitly states: "Read mode ... sits behind a `Renderer` trait, because
iced's markdown widget may not cover tables, footnotes, or images well enough."

This finding confirms that decision: **iced's markdown widget is insufficient**
for Flavite's read mode because it does not render:
- Wikilinks (the "wikilink" event from pulldown-cmark with a `wiki:` URL
  scheme is not recognized)
- Embeds (the image embed syntax)
- Task lists (checkboxes)

## Decision

**Decision:** Flavite implements a custom `Renderer` trait and a custom read-mode
renderer that:

1. Uses pulldown-cmark for parsing (already a dependency, §4.3).
2. Handles wikilinks (`[[note]]`) by rendering them as clickable links
   (using the same scanner semantics as the editor — D10).
3. Handles embeds (`![[image.png]]`) by rendering the attachment inline or
   as a clickable file card (R3).
4. Handles task lists (`- [x]`) by rendering them as interactive checkboxes.
5. Handles tables by rendering them as simple grid layouts.
6. Falls back to iced's markdown widget for the features it does support
   (headings, paragraphs, emphasis, code spans, regular lists, blockquotes,
   code fences), or replaces it entirely with a custom renderer.

### Architecture

```text
flavite-syntax (scanner) ──► custom Link/Embed/Tags detection
                │
                ▼
pulldown-cmark parser ──► custom event handler (Renderer impl)
                │
                ▼
custom widget with:
  - text runs (paragraphs, headings)
  - link runs (wikilinks, URLs)
  - embed runs (attachments)
  - code runs (fenced, inline)
  - task list runs (checkboxes)
  - table runs (grids)
```

The custom renderer:
- Is **CPU-only** (no GPU required for read mode).
- Reuses pulldown-cmark's event stream (Flavite already depends on it for
  indexing).
- Shapes text with cosmic-text (already a transitive dependency via iced).
- Renders inline images via the `image` crate (already a dependency).
- Delegates rendering to iced's primitives (cosmic-text for text, custom
  drawing for everything else).

### Ownership

The custom renderer belongs in `flavite-editor` (P6) as the read-mode surface.
It consumes:
- `flavite-syntax` for link/embed/math detection (D10)
- `flavite-api` for attachment resolution
- `pulldown-cmark` for CommonMark parsing
- `cosmic-text` for text shaping
- `image` for image decoding

### Non-decisions

- **Math rendering** is explicitly deferred (§4.10: "Math spans MUST be parsed,
  excluded from link and tag extraction, and shown as styled source"). A
  future typst-based renderer is the likely candidate (R1).
- **Syntax highlighting for code fences** is not a v1 requirement.

## Reversal conditions

Revisit if:
1. A future iced version adds wikilink/embed/task-list support to its
   markdown widget.
2. The custom renderer proves too costly to maintain.

Both conditions are **unlikely** and **not blocking** for v1.
