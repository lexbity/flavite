"""FEUX-0002 X06: match-driven selection in a real session.

Activates the owned selection-search row from the **command palette**
(keyboard path to the same executor the Select-menu rows use); drafts a
literal query, proves the overlay paints without mutating the committed
set, Apply selects both matches, typing across members edits both, undo
restores the exact text, and mode exit keeps the standing count. The
Select-menu rows themselves are asserted as rendered bounds (the native
menu-row click path is unreliable under this environment's synthetic
pointer timing — recorded in the X04 verification record). The embedded
negative is the overlay paint: a broken preview that commits on draft
(instead of outlining) changes the committed selection, and the validator
fails.
"""
import time

import common
from context_mode import painted


def search_state(s):
    return next(
        (t.get("selection_search") for t in common.tabs(s) if t.get("selection_search") is not None),
        None,
    )


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    wait(
        lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "source editor ready",
    )
    value = common.snapshot(harness)
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")

    common.key("ctrl+End")
    common.type_text("TODO one")
    common.key("Return")
    common.type_text("TODO two")
    common.key("Return")
    common.type_text("plain")
    wait(
        lambda s: s.get("focused_text", "").endswith("plain"),
        "three-line source text",
    )
    before = common.snapshot(harness)["focused_text"]
    common.key("ctrl+Home")
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Surface: the Select menu exposes both match-selection rows. The menu
    # row bounds are harness-rendered, so both rows are proven to exist
    # (X06 surface); activation below goes through the palette (keyboard)
    # because menu-row clicks do not dispatch reliably under this
    # environment's synthetic pointer timing (X04 record).
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if "Select matches…" in bounds and "Split on matches…" in bounds:
            break
        common.key("v")
        time.sleep(0.1)
    assert "Select matches…" in bounds and "Split on matches…" in bounds, (
        "Select menu must expose both match-selection rows"
    )
    # Close the menu again (Escape pops it; a stray open menu would change
    # later key routing).
    if (common.snapshot(harness).get("context_mode") or {}).get("menu"):
        common.key("Escape")
        wait(
            lambda s: (s.get("context_mode") or {}).get("menu") is None,
            "Select menu closed",
        )

    # Activate "Select matches…" via the palette (same executor as the row).
    common.key("ctrl+shift+p")
    wait(lambda s: s.get("palette_open"), "palette opened")
    common.key("ctrl+a")
    common.type_text("editor.select_matches")
    common.key("Return")
    wait(lambda s: not s.get("palette_open"), "palette dispatched")
    wait(
        lambda s: (search_state(s) or {}).get("kind") == "select",
        "selection-search row open",
    )
    # Type the query; verify it lands in the query input (the row's focus
    # task is asynchronous; leaked keystrokes would go into the editor).
    assert common.type_into_selection_row(window, harness, "TODO", "select"), (
        "query did not land in the selection-search row"
    )
    wait(
        lambda s: (search_state(s) or {}).get("pending") is False
        and (search_state(s) or {}).get("overlay_matches") == 2,
        "draft collected two matches",
    )
    # The overlay paints without committing: the committed set stays one.
    wait(lambda s: common.selections(s) == 1, "overlay is preview-only")
    common.capture(window, output, "01-match-overlay.png", checks)

    # Return applies (query-input binding); the committed set becomes the
    # two matches.
    common.key("Return")
    wait(lambda s: common.selections(s) == 2, "apply selects both matches")
    assert common.snapshot(harness)["focused_text"] == before, "apply is selection-only"

    # Close the row, then type across both members: one insert lands in
    # each match (a single atomic batch).
    common.key("Escape")
    wait(
        lambda s: (search_state(s) or {}).get("kind") != "select",
        "search row closed",
    )
    # The typing entry can be dropped by the synthetic-input capture chain
    # (bounded retry — the byte-exact assertions below stay strict).
    entered = False
    for _ in range(4):
        common.key("i")
        end = time.monotonic() + 4
        while time.monotonic() < end:
            if (common.snapshot(harness).get("context_mode") or {}).get("state") == "TYPING":
                entered = True
                break
            time.sleep(0.1)
        if entered:
            break
        time.sleep(0.4)
    assert entered, "typing group did not open"
    common.type_text("X")
    wait(
        lambda s: "XTODO one" in s.get("focused_text", "")
        and "XTODO two" in s.get("focused_text", ""),
        "typing edited both members",
    )
    common.key("Escape")

    # Undo restores the exact text.
    common.key("ctrl+z")
    wait(
        lambda s: s.get("focused_text") == before,
        "undo restored the exact text",
    )

    # (No residue Escape here: the row already closed above, and Command
    # Escape with N>1 keeps the primary — §5.4 row 10 — which would
    # collapse the set this scenario must see surviving exit.)
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") in (None, "Off"),
        "mode exited",
    )
    wait(lambda s: common.selections(s) == 2, "selections survive exit")

    checks.append({"id": "context-mode-matches-overlay-apply-undo", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
