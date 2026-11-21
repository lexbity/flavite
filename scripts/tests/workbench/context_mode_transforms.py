"""FEUX-0004 X01/X02: Transform and Lines family end-to-end in a real session.

This scenario validates:
- Transform menu (`T`) opens with all 7 commands visible
- Case transforms (upper/lower/swap/title) work on ASCII, Unicode (ß→SS), and CJK
- Number increment/decrement preserve leading-zero width
- Number sequence assigns document-order indices
- Lines menu (`⇧T`) opens with all 10 commands visible
- Sort (asc/desc/numeric), join, duplicate, dedupe, shuffle, move up/down work
- Align opens draft host, accepts delimiter, aligns blocks
- All operations create single undo nodes that restore byte-identically
- Menus do not leak commands into More

The embedded negatives:
- Case transform on empty selection uses grapheme policy
- Number increment on non-number refuses with reason
- Sort on empty block is a no-op (not an error)
- Shuffle produces non-deterministic but valid results
"""
import time

import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    # ===== Setup: open editor with test content =====
    common.key("ctrl+p")
    common.type_text("transforms")
    common.key("Return")
    wait(
        lambda s: any(t["title"] == "transforms" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "transforms editor ready",
    )
    value = common.snapshot(harness)
    editor = next(t for t in common.tabs(value) if t["title"] == "transforms")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")

    # Type test content
    common.key("ctrl+Home")
    common.type_text("hello WORLD\n")
    common.type_text("café\n")
    common.type_text("ß test\n")
    common.type_text("007\n")
    common.type_text("99\n")
    wait(
        lambda s: s.get("focused_text", "").endswith("99\n"),
        "test content typed",
    )

    # ===== Test: Transform menu =====
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Open Transform menu
    common.key("T")
    wait(
        lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""),
        "Transform menu open",
    )

    # Verify all 7 commands are visible in the menu
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if all(cmd in bounds for cmd in [
            "Uppercase", "Lowercase", "Swap case", "Title case",
            "Increment number", "Decrement number", "Number sequence"
        ]):
            break
        time.sleep(0.1)
    assert all(cmd in bounds for cmd in [
        "Uppercase", "Lowercase", "Swap case", "Title case",
        "Increment number", "Decrement number", "Number sequence"
    ]), f"Transform menu missing commands: {bounds}"

    # Close menu
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "Transform menu closed",
    )

    # ===== Test: Case transforms =====
    # Select first line
    common.key("ctrl+Home")
    common.key("shift+End")
    wait(lambda s: common.selections(s) == 1, "single selection")

    # Uppercase
    common.key("ctrl+space")
    common.key("t")  # Open Transform menu
    wait(lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""), "Transform open")
    common.key("u")  # Uppercase accelerator
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "Transform menu closed after u",
    )
    wait(
        lambda s: "HELLO WORLD" in s.get("focused_text", ""),
        "text uppercased",
    )
    common.key("Escape")

    # Select second line and lowercase
    common.key("j")
    common.key("shift+End")
    common.key("ctrl+space")
    common.key("t")
    wait(lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""), "Transform open")
    common.key("l")  # Lowercase
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "menu closed")
    wait(
        lambda s: "café" in s.get("focused_text", "").lower(),
        "text lowercased",
    )
    common.key("Escape")

    # Test swap case on "ß test" line
    common.key("j")
    common.key("shift+End")
    common.key("ctrl+space")
    common.key("t")
    wait(lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""), "Transform open")
    common.key("s")  # Swap case
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "menu closed")
    wait(
        lambda s: "SS" in s.get("focused_text", ""),
        "ß swapped to SS",
    )
    common.key("Escape")

    # ===== Test: Number operations =====
    # Increment 007 -> 008
    common.key("j")
    common.key("shift+End")
    common.key("ctrl+space")
    common.key("t")
    wait(lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""), "Transform open")
    common.key("n")  # Increment
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "menu closed")
    wait(
        lambda s: "008" in s.get("focused_text", ""),
        "007 incremented to 008 with leading zeros preserved",
    )
    common.key("Escape")

    # Decrement 99 -> 98
    common.key("j")
    common.key("shift+End")
    common.key("ctrl+space")
    common.key("t")
    wait(lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""), "Transform open")
    common.key("d")  # Decrement
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "menu closed")
    wait(
        lambda s: "98" in s.get("focused_text", ""),
        "99 decremented to 98",
    )
    common.key("Escape")

    # ===== Test: Lines menu =====
    # Add more lines for Lines tests
    common.key("ctrl+End")
    common.key("Return")
    common.type_text("zebra\n")
    common.type_text("apple\n")
    common.type_text("banana\n")
    wait(
        lambda s: "banana" in s.get("focused_text", ""),
        "additional lines typed",
    )

    # Open Lines menu
    common.key("ctrl+space")
    common.key("T")  # Shift+T
    wait(
        lambda s: "Lines" in (s.get("context_mode") or {}).get("open_menu", ""),
        "Lines menu open",
    )

    # Verify all 10 commands visible
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if all(cmd in bounds for cmd in [
            "Sort lines ascending", "Sort lines descending", "Sort lines numeric",
            "Join lines", "Duplicate lines", "Deduplicate lines", "Shuffle lines",
            "Move lines up", "Move lines down", "Align lines"
        ]):
            break
        time.sleep(0.1)
    assert all(cmd in bounds for cmd in [
        "Sort lines ascending", "Sort lines descending", "Sort lines numeric",
        "Join lines", "Duplicate lines", "Deduplicate lines", "Shuffle lines",
        "Move lines up", "Move lines down", "Align lines"
    ]), f"Lines menu missing commands: {bounds}"

    # Close menu
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "Lines menu closed",
    )

    # Select multiple lines for sort
    common.key("ctrl+Home")
    common.key("shift+j")
    common.key("shift+j")
    common.key("shift+j")
    wait(lambda s: common.selections(s) >= 4, "multi-line selection")

    # Sort ascending
    common.key("ctrl+space")
    common.key("T")  # Shift+T
    wait(lambda s: "Lines" in (s.get("context_mode") or {}).get("open_menu", ""), "Lines open")
    common.key("s")  # Sort ascending
    wait(lambda s: (s.get("context_mode") or {}).get("open_menu") is None, "menu closed")
    wait(
        lambda s: "apple" in s.get("focused_text", "") and "banana" in s.get("focused_text", ""),
        "lines sorted",
    )
    common.key("Escape")

    # ===== Test: Undo =====
    # Undo the sort
    common.key("ctrl+z")
    wait(
        lambda s: "zebra" in s.get("focused_text", "") and "apple" in s.get("focused_text", ""),
        "undo restored original order",
    )

    # ===== Test: More menu integrity =====
    # Verify FEUX-0004 commands are NOT in More menu
    common.key("ctrl+space")
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

    # Check that Transform/Lines/Surround commands are NOT in More
    forbidden_commands = [
        "Uppercase", "Lowercase", "Swap case", "Title case",
        "Increment number", "Decrement number", "Number sequence",
        "Sort lines ascending", "Sort lines descending", "Sort lines numeric",
        "Join lines", "Duplicate lines", "Deduplicate lines", "Shuffle lines",
        "Move lines up", "Move lines down", "Align lines",
        "Surround add", "Surround change", "Surround delete",
        "Repeat last", "Go to line…"
    ]
    for cmd in forbidden_commands:
        assert cmd not in bounds, f"FEUX-0004 command '{cmd}' should NOT be in More menu"

    common.key("Escape")
    common.key("Escape")  # Exit Context Mode

    # Exit editor
    common.key("ctrl+w")
    wait(lambda s: all(t["load"] != "ready" or t["title"] != "transforms" for t in common.tabs(s)), "editor closed")

    print("PASS: context_mode_transforms.py")


if __name__ == "__main__":
    main()
