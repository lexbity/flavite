"""FEUX-0002 X05: pure set ops and find-char in a real session.

Builds a 3-member set via the real Select menu, proves `editor.collapse`
(keep heads) vs `editor.clear_selection` (single caret at primary head)
differ exactly as specified at N>1, then drives `f` find-char through the
`AwaitChar` state with a real produced character, including the Esc-cancel
row. The embedded negative is the find-char cancel: a broken state that
commits on Esc would change the text and the validator fails.
"""
import time

import common
from context_mode import painted


def selections(s):
    editor = next(t for t in common.tabs(s) if t.get("selection_count") is not None)
    return editor["selection_count"]


def settled(harness, count, label):
    """Two consecutive snapshots at `count` prove the publish frame landed
    before the next menu opens (the toolbar captures its target per frame)."""
    end = time.monotonic() + 15
    previous = None
    while time.monotonic() < end:
        current = selections(common.snapshot(harness))
        if current == count and previous == count:
            return
        previous = current
        time.sleep(0.04)
    raise AssertionError(f"{label}: never settled at {count}")


def menu_click(harness, wait, label, row):
    """Open the Select menu with `v` (verified per attempt — the
    acknowledged-input guard can drop the first captured key after a
    selection-only command), navigate with Down/Return, then close any
    residue and wait for the overlay to fully close."""
    opened = False
    for _ in range(6):
        common.key("v")
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            if (common.snapshot(harness).get("context_mode") or {}).get("menu") == "Select":
                opened = True
                break
            time.sleep(0.04)
        if opened:
            break
    assert opened, f"{label}: Select menu never opened"
    for _ in range(row):
        common.key("Down")
    common.key("Return")
    common.key("Escape")
    previous = None
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        closed = (common.snapshot(harness).get("context_mode") or {}).get("menu") is None
        if closed and previous:
            return
        previous = closed
        time.sleep(0.04)
    raise AssertionError(f"{label}: menu overlay never closed")


def snapshot_bounds(harness, label):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        s = common.snapshot(harness)
        bounds = (s.get("context_mode") or {}).get("bounds") or {}
        if label in bounds:
            return bounds[label]
        time.sleep(0.04)
    raise AssertionError(f"{label} control not rendered")


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(
        lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "source editor ready",
    )
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    # Widen the window so the Select/Go menus pass width admission (≥720)
    # and can be opened through their toolbar buttons.
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")
    common.key("ctrl+End")
    for word in ("one", "two", "three"):
        common.type_text(word)
        if word != "three":
            common.key("Return")
    wait(
        lambda s: s.get("focused_text", "").endswith("three"),
        "three-line source text",
    )
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Build 3 members.
    for expected in (2, 3):
        menu_click(harness, wait, "Add caret on row below", 7)
        wait(lambda s, e=expected: selections(s) == e, f"{expected} selections")
        settled(harness, expected, f"{expected} selections settled")

    # Collapse: every range -> caret at head (here all carets stay).
    menu_click(harness, wait, "Collapse selections", 3)
    wait(lambda s: selections(s) == 3, "collapse keeps heads at N>1")

    # Clear selection: one caret at the primary head (E-7 distinction).
    menu_click(harness, wait, "Clear selection", 4)
    wait(lambda s: selections(s) == 1, "clear collapses to one caret")
    text_after_clear = common.snapshot(harness)["focused_text"]

    # Rebuild to 3 for find-char.
    for expected in (2, 3):
        menu_click(harness, wait, "Add caret on row below", 7)
        wait(lambda s, e=expected: selections(s) == e, f"rebuild {expected}")
        settled(harness, expected, f"rebuild {expected} settled")

    # Find-char: `f` opens AwaitChar; OCR the capture hint.
    common.key("f")
    wait(
        lambda s: (s.get("context_mode") or {}).get("message") == "Waiting for character…",
        "AwaitChar hint",
    )
    value = painted(window, harness, output, "Waiting for character")

    # Esc cancels the capture (the embedded negative): nothing changes.
    before = common.snapshot(harness)["focused_text"]
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND"
        and (s.get("context_mode") or {}).get("message") != "Waiting for character…",
        "find-char cancelled",
    )
    assert common.snapshot(harness)["focused_text"] == before
    assert selections(common.snapshot(harness)) == 3

    # `f` then a produced character resolves: 'r' exists at 4 (in
    # "# Alpha") and at 32 (in "three") after the heads at 0, 8, 9, so
    # the members move to {4, 32} — selection-only, no text mutation.
    common.key("f")
    common.type_text("r")
    wait(
        lambda s: selections(s) == 2 and s.get("focused_text") == before,
        "find-char resolved without text mutation",
    )

    checks.append({"id": "context-mode-setops-collapse-clear-findchar", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
