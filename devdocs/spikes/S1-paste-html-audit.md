# Spike S1-2: Paste-from-HTML sanitization audit

**Status:** closed. **Slice:** PLAN-A A6 (§9 Phase 0, spike S1, part 2 of 2).
**Date:** 2026-10-26. **Pinned versions:** iced 0.14.0, winit 0.30.13.

## Question

What is the paste-from-HTML path for the custom editor? The clipboard can contain
HTML formatted text, and the spec requires (§4.10): "No raw HTML ever rendered or
executed." We must decide how to handle HTML paste events.

## Findings

### F1 — iced's Clipboard trait is text-only at 0.14

The `iced_core::Clipboard` trait (the abstraction the custom widget receives) has
only:

```rust
pub trait Clipboard {
    fn read(&self, kind: Kind) -> Option<String>;
    fn write(&mut self, kind: Kind, contents: String);
}
```

The `Kind` enum has only `Standard` and `Primary` — there is no `Html` variant or
format parameter. The implementation in `iced_winit` calls the OS's
text-clipboard API, not HTML.

Consequence: **a custom iced widget receives only plain text from the
clipboard at the pinned iced version.** HTML paste is not available through
iced's abstraction.

### F2 — Platform-specific HTML clipboard access is out of scope

winit 0.30 exposes `window.set_clipboard()` / `.with_clipboard()` for the
text clipboard only; it does not surface HTML or other MIME types. Accessing
HTML would require platform-specific calls:

- Windows: `CF_HTML` format via `OpenClipboard` / `GetClipboardData`
- macOS: `kUTTypeHTML` via `NSPasteboard`
- Linux/X11: `text/html` via the `CLIPBOARD` selection
- Linux/Wayland: `text/html` via the data offer

These are outside the iced/winit abstraction and would require unsafe FFI
or platform-specific dependencies (winapi, cocoa, x11, wayland-client), which
violates the project's pure-Rust constraint.

### F3 — Browser-grade HTML sanitization in pure Rust is heavy

A production-grade HTML sanitizer (like DOMPurify) is a large JS library with
a browser-grade HTML parser. Pure-Rust options:

- `html5ever` (servo's parser) + manual sanitization: large, slow for
  interactive use, and not designed for security (DOMPurify's HTML is
  hardened).
- `lol_html`: a Rust HTML rewriting library with sanitization support.
  License: MIT OR Apache-2.0. repository: https://github.comCloudflare/lol_html.
  Dependencies: `html5ever` (Servo's HTML parser), `tendril`,
  `markup5ever_rcdom`.
  The lol_html allowlist is designed for security.

`lol_html` is on crates.io and is compatible with our deny.toml allowlist
(§10.4: MIT, Apache-2.0 are allowed).

### F4 — Alternative: text-fallback path

If HTML paste is not available through iced's abstraction (F1), and platform
HTML access is out of scope (F2), the simplest path is: **accept only plain
text from the clipboard.** When the user pastes HTML-rich content from a
browser or word processor, the OS clipboard typically also includes a plain
text representation. iced/winit already retrieves this.

Advantages:
- Zero additional code, dependencies, or complexity.
- Matches the text-only `iced::advanced::Clipboard` contract.
- Fully compatible with the pure-Rust constraint.

Disadvantages:
- Users pasting from rich sources lose formatting (bold, italic, links
  embedded in the HTML). The spec does not require preserving formatting from
  HTML paste — it only forbids rendering raw HTML.

### F5 — The spec's position on raw HTML

§4.10: "No raw HTML ever rendered or executed." This is an absolute
requirement. Accepting only plain text from the clipboard satisfies this.

§4.10 also specifies: "URL scheme allowlist (`http`, `https`, `mailto`)."
This applies to clickable URLs in notes, not to paste content. Pasting
`https://example.com` as plain text is fine; it becomes a wikilink or bare URL
in the note, not an HTML `<a>` tag.

## Decision

**Decision:** The production editor (A11+) receives paste events via
`iced::advanced::Clipboard::read(Kind::Standard)`, which returns only plain
text. This satisfies §4.10 (no raw HTML rendered) and the pure-Rust
constraint. No HTML sanitization library is required.

### Implementation for A11

In `flavite-editor/src/widget.rs` (the editor widget's `on_event` handler):

```rust
Event::Keyboard(keyboard::Event::KeyPressed { text: Some(text), .. }) => {
    // Normal key text (non-IME, non-paste)
    if text.chars().any(char::is_control) {
        return false; // skip control chars
    }
    self.insert_at_caret(text);
    true
}

Event::Clipboard(clipboard::Event::TextCopied(text)) => {
    // Paste: insert as plain text (never HTML)
    if !text.is_empty() && !text.chars().any(char::is_control) {
        self.insert_at_caret(&text);
    }
    true
}
```

The `text` field in both cases is **guaranteed plain text** by iced's
abstraction. No sanitization is required.

### If HTML access becomes available in a future iced

If a future iced version exposes HTML clipboard formats, the decision can be
revisited. At that point, add `lol_html` to the dependencies and sanitize
before insertion. The current decision is **not a technical debt** — it is a
conscious choice to accept the text-only path that iced provides.

## Reversal conditions

Revisit if:
1. A future iced/winit adds HTML clipboard access in a pure-Rust way.
2. Users report that plain-text paste from rich sources is insufficient
   and request HTML paste with formatting preservation.

Both conditions are currently **unlikely** and **not blocking** for v1.
