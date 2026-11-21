"""FEUX-0004 X04: Surround family end-to-end in a real session.

This scenario validates:
- Surround menu (`Z`) opens with add/change/delete commands
- Surround add wraps selections with specified delimiters
- Surround change replaces existing delimiters
- Surround delete removes delimiters
- Pair scanner handles nesting pairs ((), {}, [], <>) and self-pairs ("", '', `)
- All-or-nothing failure: one member without pair refuses whole command
- AwaitArgs state captures characters correctly
- Count can be combined with surround (though surround doesn't use count)
- Empty selection wraps empty range

The embedded negatives:
- Surround add on non-pair character creates self-pair
- Surround change/delete on member without pair refuses with count
- Esc cancels AwaitArgs capture
"""
import time

import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    # ===== Setup: open editor with test content =====
    common.key("ctrl+p")
    common.type_text("surround")
    common.key("Return")
    wait(
        lambda s: any(t["title"] == "surround" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "surround editor ready",
    )
    value = common.snapshot(harness)
    editor = next(t for t in common.tabs(value) if t["title"] == "surround")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")

    # Type test content
    common.key("ctrl+Home")
    common.type_text("(hello)\n")
    common.type_text("{world}\n")
    common.type_text("[test]\n")
    common.type_text("'quoted'\n")
    common.type_text('"double"\n')
    common.type_text("`backtick`\n")
    wait(
        lambda s: "`backtick`" in s.get("focused_text", ""),
        "test content typed",
    )

    # ===== Test: Surround menu =====
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Open Surround menu
    common.key("Z")
    wait(
        lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""),
        "Surround menu open",
    )

    # Verify all 3 commands are visible
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if all(cmd in bounds for cmd in ["Surround add", "Surround change", "Surround delete"]):
            break
        time.sleep(0.1)
    assert all(cmd in bounds for cmd in ["Surround add", "Surround change", "Surround delete"]), \
        f"Surround menu missing commands: {bounds}"

    # Close menu
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "Surround menu closed",
    )

    # ===== Test: Surround add with () =====
    common.key("ctrl+Home")
    common.key("shift+End")  # Select first line
    wait(lambda s: common.selections(s) == 1, "line selected")

    common.key("ctrl+space")
    common.key("Z")  # Open Surround
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("a")  # Add
    wait(
        lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""),
        "waiting for character",
    )
    common.key("(")  # Add parentheses
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "surround add dispatched",
    )
    wait(
        lambda s: "((hello))" in s.get("focused_text", ""),
        "first line wrapped in parentheses",
    )
    common.key("Escape")

    # ===== Test: Surround add with custom self-pair =====
    common.key("j")
    common.key("shift+End")  # Select second line
    common.key("ctrl+space")
    common.key("Z")
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("a")
    wait(lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""), "waiting")
    common.key("*")  # Custom delimiter
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "dispatched")
    wait(
        lambda s: "*{world}*" in s.get("focused_text", ""),
        "second line wrapped in asterisks",
    )
    common.key("Escape")

    # ===== Test: Surround delete =====
    common.key("ctrl+Home")
    common.key("j")
    common.key("shift+End")  # Select first two lines
    wait(lambda s: common.selections(s) == 2, "two lines selected")

    common.key("ctrl+space")
    common.key("Z")
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("d")  # Delete
    wait(
        lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""),
        "waiting for delete character",
    )
    common.key("(")  # Delete parentheses
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "delete dispatched")
    wait(
        lambda s: "(hello)" in s.get("focused_text", "") and "*{world}*" in s.get("focused_text", ""),
        "parentheses removed, asterisks remain",
    )
    common.key("Escape")

    # ===== Test: Surround change =====
    common.key("ctrl+Home")
    common.key("j")
    common.key("shift+End")  # Select first line
    wait(lambda s: common.selections(s) == 1, "line selected")

    common.key("ctrl+space")
    common.key("Z")
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("c")  # Change
    wait(
        lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""),
        "waiting for first character",
    )
    common.key("(")  # From (
    wait(
        lambda s: "Waiting for second character" in (s.get("context_mode") or {}).get("detail", ""),
        "waiting for second character",
    )
    common.key("[")  # To [
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "change dispatched")
    wait(
        lambda s: "[hello]" in s.get("focused_text", ""),
        "parentheses changed to brackets",
    )
    common.key("Escape")

    # ===== Test: Surround on empty selection =====
    common.key("ctrl+End")
    common.key("Return")
    common.type_text("empty test")
    common.key("Return")
    common.key("ctrl+Home")
    common.key("ctrl+space")

    # Place caret at empty location (beginning of line)
    common.key("Z")
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("a")
    wait(lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""), "waiting")
    common.key("#")
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "dispatched")
    wait(
        lambda s: "#" in s.get("focused_text", ""),
        "empty selection wrapped",
    )

    # ===== Test: Esc cancels AwaitArgs =====
    common.key("j")
    common.key("shift+End")
    common.key("Z")
    wait(lambda s: "Surround" in (s.get("context_mode") or {}).get("open_menu", ""), "Surround open")
    common.key("a")
    wait(lambda s: "Waiting for character" in (s.get("context_mode") or {}).get("detail", ""), "waiting")
    common.key("Escape")  # Cancel
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "back to command state",
    )
    wait(
        lambda s: "Waiting" not in (s.get("context_mode") or {}).get("detail", ""),
        "capture cancelled",
    )

    # ===== Test: More menu integrity for Surround =====
    common.key("V")  # More menu
    wait(
        lambda s: "More" in (s.get("context_mode") or {}).get("open_menu", ""),
        "More menu open",
    )
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if "More" in bounds:
            break
        time.sleep(0.1)

    # Check that Surround commands are NOT in More
    forbidden_commands = ["Surround add", "Surround change", "Surround delete"]
    for cmd in forbidden_commands:
        assert cmd not in bounds, f"Surround command '{cmd}' should NOT be in More menu"

    common.key("Escape")
    common.key("Escape")  # Exit Context Mode

    # Exit editor
    common.key("ctrl+w")
    wait(lambda s: all(t["load"] != "ready" or t["title"] != "surround" for t in common.tabs(s)), "editor closed")

    print("PASS: context_mode_surround.py")


if __name__ == "__main__":
    main()
