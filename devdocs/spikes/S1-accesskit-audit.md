# Spike S1-2: AccessKit accessibility audit

**Status:** closed. **Slice:** PLAN-A A6 (§9 Phase 0, spike S1, part 2 of 2).
**Date:** 2026-10-26. **Pinned versions:** iced 0.14.0, winit 0.30.13.

## Question

Can iced's accessibility integration expose text-range nodes + caret from a
custom widget for screen reader support (D18: screen-reader support is a SHOULD
gated on S1)? If not, do we contribute upstream or document the gap?

## Findings

### F1 — iced 0.14 has no AccessKit integration

As of iced 0.14.0, **iced has no AccessKit integration at all.** Searching
the iced workspace (iced_core, iced_winit, iced_runtime) for "accesskit" or
"AccessKit" returns zero matches.

iced's approach to accessibility is currently **nonexistent** in the public
API. The `Widget` trait does not expose any accessibility hooks.

### F2 — iced's widget tree is a custom trait

iced's `Widget` trait is defined in `iced_core::widget` and is object-safe
(`&dyn Widget`). It has no accessibility methods. The widget tree is
constructed by the application and updated by the runtime; there is no hook
for injecting AccessKit nodes.

### F3 — AccessKit crate status

The `accesskit` crate (https://github.com/AccessKit/accesskit) provides
platform-agnostic accessibility APIs for Rust applications. It has:

- `accesskit` — the core types and node tree
- `accesskit-winit` — winit platform integration (Windows: UIA/IA2, macOS:
  NSAccessibility, Linux: AT-SPI)
- Example integrations with various Rust GUI toolkits

`accesskit-winit` is designed to be integrated by the windowing/rendering layer,
not by individual widgets. The typical pattern:

1. Application creates an `accesskit::Tree` and `accesskit::Node` hierarchy
   that mirrors the widget tree.
2. On each frame, the application updates the AccessKit tree with the current
   widget state.
3. `accesskit-winit` sends the tree to the platform's accessibility API.

### F4 — Integration point for iced

To integrate AccessKit with iced, we would need to:

1. Add `accesskit` and `accesskit-winit` as dependencies.
2. Add a method to `iced_core::Widget` that returns an `accesskit::Node`
   for the widget's current state (or keep accessibility separate from
   the Widget trait entirely, as AccessKit's example integrations do).
3. Walk the widget tree after each frame and build/update the AccessKit tree.
4. Send the tree to `accesskit-winit` for platform delivery.

This is **not a widget-level change** — it is a runtime-level change to iced
itself. A downstream crate (like Flavite) cannot add AccessKit support to a
custom widget without upstream changes.

### F5 — iced's roadmap

As of 2026-10-26, iced's GitHub roadmap does not list AccessKit integration as
an active milestone. The maintainers have not expressed a timeline for adding it.

### F6 — Workaround: text-only accessibility via platform APIs

On each platform, a custom widget could theoretically expose accessibility
via platform-specific APIs:

- Windows: Implement `IAccessible` or `IRawElementProviderSimple` via FFI.
- macOS: Implement `NSAccessibility` protocol via objc FFI.
- Linux: Register with AT-SPI via D-Bus.

This violates the pure-Rust constraint and is not maintainable for a
text editor widget.

## Decision

**Decision:** Document the gap and track it as a SHOULD (D18: screen-reader
support is a SHOULD gated on S1). The gap is **not in Flavite** — it is in
iced itself, which lacks any AccessKit integration at the pinned version.

### Concrete actions

1. **Record the waiver:** `devdocs/spikes/S1-verdict.md` records that screen
   reader support is **waived pending iced upstream AccessKit integration**.

2. **Track upstream:** Open or upvote an iced issue requesting AccessKit
   integration. Reference: 
   https://github.com/iced-rs/iced/issues/XXXXX (to be filed).

3. **Label honestly:** The `flavite-editor` crate documents that its custom
   widget does not expose accessibility nodes because iced does not provide
   the abstraction.

4. **Revisit trigger:** When iced adds AccessKit support (or when the lack of
   it blocks real users), this decision is revisited. The custom widget
   implementation (A7–A11) is designed so that adding AccessKit integration
   later is a **non-invasive change** — we control the widget tree and can
   build the AccessKit tree in parallel.

### What Flavite can do today

- **Keyboard navigation** works: the custom widget handles arrow keys,
  Home/End, selection, etc. Screen reader users can navigate with the
  keyboard.
- **Text content is accessible** via the OS clipboard: copy/paste works.
- **The widget tree is navigable** via tab focus (iced's focus system).

What is **missing**: screen readers cannot read the text content or announce
caret position changes because there are no accessibility nodes.

### Waiver text for `devdocs/spikes/S1-verdict.md`

```markdown
| Checklist item | Decision | Verdict |
|---|---|---|
| Screen reader support | iced lacks AccessKit integration at 0.14; file upstream issue | **Waived** — tracked, not blocking |
```

## Reversal conditions

Revisit if:
1. iced adds AccessKit integration (or a different accessibility abstraction).
2. A real screen-reader user reports that the lack of support blocks them
   from using Flavite.
3. A funded initiative exists to implement accessibility upstream in iced.

At that point, the cost of integrating AccessKit is **low** because the
widget tree is already controlled by Flavite.
