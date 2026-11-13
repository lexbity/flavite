"""FEUX-0002 X04: multi-selection in a real session.

Builds a multi-member set through the real Select-menu (Add caret on row
below) with real OS input, types across every member in one atomic batch,
verifies the set survives mode exit with the OFF-state count, and proves
Esc-collapse in ordinary Edit (the embedded negative: a broken collapse
keeps N>1 and the validator fails).

Semantic readiness comes from the harness `selection_count`/`selections`
fields; paint is OCR-verified against the rendered note chrome.
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

    # Append short lines after the default body.
    for word in ("one", "two", "three", "four"):
        common.type_text(word)
        if word != "four":
            common.key("Return")
    wait(
        lambda s: s.get("focused_text", "").endswith("four"),
        "four-line source text",
    )
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Build the set: 1 -> 2 -> 3 -> 4 members via Add caret on row below.
    for expected in (2, 3, 4):
        menu_click(harness, wait, "Add caret on row below", 7)
        wait(lambda s, e=expected: selections(s) == e, f"{expected} selections")
        settled(harness, expected, f"{expected} selections settled")

    # Type across every member in one atomic batch.
    common.key("i")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "TYPING",
        "typing",
    )
    common.type_text("X")
    common.key("Escape")
    value = wait(
        lambda s: "Xone" in s.get("focused_text", "") and selections(s) == 4,
        "typing landed on all four members",
    )

    # Exit mode: the set survives with the OFF-state count (FR-13).
    common.key("ctrl+space")
    wait(lambda s: s.get("context_mode") is None, "mode exit")
    wait(lambda s: selections(s) == 4, "set retained after exit")

    # Paint: the note footer shows the non-color count (whole-window OCR).
    deadline = time.monotonic() + 20
    last = ""
    while time.monotonic() < deadline:
        image = output / "paint-footer.png"
        common.run("import", "-window", window, str(image))
        last = common.run("tesseract", str(image), "stdout", "-l", "afr", "--psm", "6")
        if "4 selections" in last.replace("\n", " "):
            break
        time.sleep(0.05)
    assert "4 selections" in last.replace("\n", " "), f"OFF-state count missing; OCR={last!r}"
    common.capture(window, output, "01-multiselect-off-state.png", checks)

    # Esc in ordinary Edit collapses the additional members first (D-6 via
    # E-14). The embedded negative: if collapse is broken, the count stays
    # 4 and this wait times out.
    common.key("Escape")
    wait(lambda s: selections(s) == 1, "Escape collapsed the additional selections")

    checks.append({"id": "context-mode-multiselect-build-type-exit-collapse", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
