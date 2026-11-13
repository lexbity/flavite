"""FEUX-0002 X06: match-driven selection in a real session.

Opens the owned selection-search row from the Select menu, drafts a literal
query, proves the overlay paints without mutating the committed set, Apply
selects both matches, typing across members edits both, undo restores the
exact text, and mode exit keeps the standing count. The embedded negative
is the overlay paint: a broken preview that commits on draft (instead of
outlining) changes the committed selection, and the validator fails.
"""
import time

import common
from context_mode import painted


def selections(s):
    editor = next(t for t in common.tabs(s) if t.get("selection_count") is not None)
    return editor["selection_count"]


def search_state(s):
    editor = next(t for t in common.tabs(s) if t.get("selection_search") is not None)
    return editor["selection_search"]


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

    # Open the Select menu; the menu row bounds are harness-rendered, so
    # both match-selection rows are proven to exist (X06 surface). The
    # acknowledged-input guard can drop the first captured `v`, so retry.
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
    # Activate "Select matches…" by clicking its row.
    common.click_rect(window, harness, bounds["Select matches…"])
    wait(
        lambda s: search_state(s).get("kind") == "select",
        "selection-search row open",
    )
    # Type the query into the query input (focused by Open); the draft
    # collects.
    common.type_text("TODO")
    wait(
        lambda s: search_state(s).get("pending") is False
        and search_state(s).get("overlay_matches") == 2,
        "draft collected two matches",
    )
    # The overlay paints without committing: the committed set stays one.
    wait(lambda s: selections(s) == 1, "overlay is preview-only")
    value = painted(window, harness, output, "TODO")

    # Return applies (query-input binding); the committed set becomes the
    # two matches.
    common.key("Return")
    wait(lambda s: selections(s) == 2, "apply selects both matches")
    assert common.snapshot(harness)["focused_text"] == before, "apply is selection-only"

    # Type across both members: one insert lands in each match.
    common.key("ctrl+Home")
    common.key("X")
    wait(
        lambda s: "XTODO one" in s.get("focused_text", "")
        and "XTODO two" in s.get("focused_text", ""),
        "typing edited both members",
    )

    # Undo restores the exact text.
    common.key("ctrl+z")
    wait(
        lambda s: s.get("focused_text") == before,
        "undo restored the exact text",
    )

    # The row closes with Escape; the toggle exits the mode; the standing
    # count survives.
    common.key("Escape")
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") in (None, "Off"),
        "mode exited",
    )
    wait(lambda s: selections(s) == 2, "selections survive exit")

    checks.append({"id": "context-mode-matches-overlay-apply-undo", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
