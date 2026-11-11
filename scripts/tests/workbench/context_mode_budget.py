"""FEUX-0002 X01: history resource closure in a real session.

Drives the real app under a harness-shrunk retained-history budget
(FLAVITE_HARNESS_HISTORY_BUDGET_BYTES): typing history grows past the
budget, the chooser reports truncation (state + paint), a surviving chip
restores, and the disk baseline stays untouched.

The seeded negative is intrinsic: if the budget override is absent (env
dropped), no eviction occurs, `history_truncated` stays false, and the
truncation assertion fails — the scenario itself is the negative validator.
"""
import time

import common
from context_mode import painted


def open_history(harness, wait):
    common.key("ctrl+shift+p")
    common.type_text("editor.history")
    common.key("Return")
    return wait(lambda s: history(s).get("open"), "owned editing history chooser")


def history(s):
    return (s.get("context_mode") or {}).get("history", {})


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(
        lambda s: any(t["title"] == "alpha" and t["editor_bounds"] for t in common.tabs(s)),
        "note opened",
    )
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    original = value["focused_text"]
    initial_disk = (vault / "alpha.md").read_text()

    common.key("ctrl+End")
    common.key("ctrl+space")
    wait(lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND", "command state")

    # Grow history with real typing: each burst is its own sealed typing
    # group (Esc returns to Command and seals), so each iteration adds one
    # history node. Under the shrunk budget, eviction kicks in well before
    # the loop finishes.
    for i in range(70):
        common.key("i")
        common.type_text(f"l{i} ")
        common.key("Escape")
        common.key("l")
        if i % 10 == 0:
            wait(
                lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
                "command state between bursts",
            )

    value = open_history(harness, wait)
    page = history(value)["page"]
    assert page.get("history_truncated"), "truncation must be visible under the shrunk budget"
    watermark = page.get("retained_watermark", 0)
    assert watermark >= 1, f"retained watermark must advance, got {watermark}"

    # Paint check: the chooser renders the truncation notice.
    value = painted(window, harness, output, "Earlier states removed", history=True)
    common.capture(window, output, "01-budget-truncation-notice.png", checks)

    # A surviving chip still restores, and the disk baseline is untouched.
    entries = history(value)["page"]["entries"]
    survivor = next(e for e in entries if not e["current"])
    node = survivor["target"]["node"]
    value = wait(lambda s: str(node) in history(s).get("bounds", {}), "surviving chip ready")
    common.click_rect(window, harness, history(value)["bounds"][str(node)])
    value = wait(
        lambda s: not history(s).get("open") and "l" in s.get("focused_text", ""),
        "chip restore lands in the editor",
    )
    assert (vault / "alpha.md").read_text() == initial_disk, "restore must leave the disk baseline alone"

    checks.append({"id": "context-history-budget-truncation-restore", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
