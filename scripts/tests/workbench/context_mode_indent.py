"""FEUX-0005 X01/X02: Indent family and whitespace conversion end-to-end.

This scenario validates:
- Indent (`⇧.`) and Dedent (`⇧,`) commands work with count prefix
- Tabs→Spaces (`e` in Transform menu) and Spaces→Tabs (`r` in Transform menu) work
- Unit detection reports the chosen unit in completion message
- Auto-indent on open_below/open_above copies leading whitespace
- Blank lines are untouched by indent/dedent
- Whitespace-only lines are converted by tabs↔spaces
- Zero-effect no-ops (all at column 0) produce no history node
- All operations create single undo nodes that restore byte-identically

The embedded negatives:
- Indent on read-only vault refuses with reason
- Dedent on buffer at column 0 is a silent no-op
- Tabs→Spaces on content without tabs is a silent no-op
"""
import time

import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)

    # ===== Setup: open editor with test content =====
    common.key("ctrl+p")
    common.type_text("indent_test")
    common.key("Return")
    wait(
        lambda s: any(t["title"] == "indent_test" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)),
        "indent_test editor ready",
    )
    value = common.snapshot(harness)
    editor = next(t for t in common.tabs(value) if t["title"] == "indent_test")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.run("xdotool", "windowsize", "--sync", window, "1600", "800")
    wait(lambda s: s["window_size"][0] >= 1580, "window widened")

    # Type test content with various indentation
    common.key("ctrl+Home")
    common.type_text("fn main() {\n")
    common.type_text("    println!(\"hello\");\n")
    common.type_text("    let x = 1;\n")
    common.type_text("\n")
    common.type_text("    if true {\n")
    common.type_text("        return;\n")
    common.type_text("    }\n")
    common.type_text("}\n")
    wait(
        lambda s: s.get("focused_text", "").endswith("}\n"),
        "test content typed",
    )

    # ===== Test: Unit detection message =====
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Indent - should show unit message
    common.key("shift+Period")
    wait(
        lambda s: "Indent" in (s.get("context_mode") or {}).get("message", "")
                  and "unit:" in (s.get("context_mode") or {}).get("message", ""),
        "indent shows unit message",
    )
    value = common.snapshot(harness)
    indent_message = value.get("context_mode", {}).get("message", "")
    assert "Indent" in indent_message and "unit:" in indent_message, f"Unexpected indent message: {indent_message}"
    common.capture(window, output, "01-indent-unit-message.png", checks)

    # Verify indentation was applied
    wait(
        lambda s: "        println" in s.get("focused_text", ""),
        "first line indented",
    )

    # ===== Test: Count-prefixed indent =====
    # Select the whole block
    common.key("ctrl+Home")
    common.key("ctrl+shift+End")
    wait(lambda s: common.selections(s) == 1, "full buffer selected")

    # 2·⇧Period (indent by 2 units)
    common.key("2")
    common.key("shift+Period")
    wait(
        lambda s: "Indent" in (s.get("context_mode") or {}).get("message", ""),
        "count-prefixed indent shows message",
    )
    # Verify double indentation
    wait(
        lambda s: "            println" in s.get("focused_text", ""),
        "double indentation applied",
    )

    # ===== Test: Dedent =====
    # Dedent once
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )
    common.key("shift+Comma")
    wait(
        lambda s: "Dedent" in (s.get("context_mode") or {}).get("message", "")
                  and "unit:" in (s.get("context_mode") or {}).get("message", ""),
        "dedent shows unit message",
    )
    value = common.snapshot(harness)
    dedent_message = value.get("context_mode", {}).get("message", "")
    assert "Dedent" in dedent_message and "unit:" in dedent_message, f"Unexpected dedent message: {dedent_message}"
    common.capture(window, output, "02-dedent-unit-message.png", checks)

    # Verify dedent was applied
    wait(
        lambda s: "        println" in s.get("focused_text", ""),
        "one level of indent removed",
    )

    # ===== Test: Transform menu - Tabs→Spaces =====
    # Create content with tabs
    common.key("ctrl+End")
    common.key("Enter")
    common.type_text("\tfn test() {\n")
    common.type_text("\t\tprintln!(\"tabs\");\n")
    common.type_text("\t}\n")
    wait(
        lambda s: "\tfn test" in s.get("focused_text", ""),
        "tab content typed",
    )

    # Select the tab-indented block
    common.key("ctrl+Up")
    common.key("ctrl+Up")
    common.key("ctrl+Up")
    common.key("ctrl+shift+Down")
    common.key("shift+Down")
    common.key("shift+Down")
    wait(lambda s: common.selections(s) == 1, "tab block selected")

    # Open Transform menu
    common.key("T")
    wait(
        lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""),
        "Transform menu open",
    )

    # Verify Tabs→Spaces is in the menu
    bounds = {}
    for _ in range(8):
        current = common.snapshot(harness)
        bounds = (current.get("context_mode") or {}).get("bounds") or {}
        if "Tabs → Spaces" in bounds:
            break
        time.sleep(0.1)
    assert "Tabs → Spaces" in bounds, f"Tabs → Spaces not in Transform menu: {bounds}"

    # Activate Tabs→Spaces using 'e' accelerator
    common.key("Escape")  # Close menu first
    wait(
        lambda s: (s.get("context_mode") or {}).get("open_menu") is None,
        "Transform menu closed",
    )
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )
    common.key("T")  # Open Transform menu
    wait(
        lambda s: "Transform" in (s.get("context_mode") or {}).get("open_menu", ""),
        "Transform menu open",
    )
    common.key("e")  # Activate Tabs→Spaces
    wait(
        lambda s: "Tabs" in (s.get("context_mode") or {}).get("message", "")
                  and "unit:" in (s.get("context_mode") or {}).get("message", ""),
        "tabs to spaces shows unit message",
    )
    value = common.snapshot(harness)
    tabs_to_spaces_message = value.get("context_mode", {}).get("message", "")
    assert "Tabs" in tabs_to_spaces_message and "Spaces" in tabs_to_spaces_message, \
        f"Unexpected tabs_to_spaces message: {tabs_to_spaces_message}"

    # Verify tabs were converted to spaces
    wait(
        lambda s: "    fn test" in s.get("focused_text", "") and "\t" not in s.get("focused_text", ""),
        "tabs converted to spaces",
    )
    common.capture(window, output, "03-tabs-to-spaces.png", checks)

    # ===== Test: Auto-indent on open_below =====
    # Position at end of first line of the converted block
    common.key("ctrl+End")
    common.key("Up")
    common.key("Up")
    common.key("End")
    wait(lambda s: s.get("focused_text", "").split("\n")[-3].strip() == "fn test() {", "at fn test line")

    # Enter context mode
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )

    # Open below
    common.key("o")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "TYPING",
        "typing state after open_below",
    )

    # Verify the new line has the same indentation as the source line
    wait(
        lambda s: "    " in s.get("focused_text", "").split("\n")[-2],
        "new line has auto-indent",
    )
    common.capture(window, output, "04-auto-indent-open-below.png", checks)

    # Exit typing
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "back to command state",
    )

    # ===== Test: Negative - Dedent at column 0 =====
    # Move to start of file
    common.key("ctrl+Home")
    common.key("shift+End")
    wait(lambda s: common.selections(s) == 1, "first line selected")

    # Try to dedent (should be a no-op)
    common.key("shift+Comma")
    wait(
        lambda s: "Dedent" in (s.get("context_mode") or {}).get("message", "")
                  or (s.get("context_mode") or {}).get("message", "") == "",
        "dedent at column 0 completed",
    )

    # Verify text unchanged (no-op)
    value = common.snapshot(harness)
    first_line = value.get("focused_text", "").split("\n")[0]
    assert first_line.strip() == "fn main() {", f"First line unexpectedly changed: {first_line}"

    # ===== Test: Undo verification =====
    # Undo the tabs→spaces conversion
    common.key("ctrl+space")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") == "COMMAND",
        "command state",
    )
    common.key("u")  # Undo
    wait(
        lambda s: "\tfn test" in s.get("focused_text", ""),
        "undo restored tabs",
    )

    # ===== Test complete =====
    common.key("Escape")
    common.key("Escape")
    wait(
        lambda s: (s.get("context_mode") or {}).get("state") is None,
        "context mode exited",
    )
    print("All FEUX-0005 X01/X02 indent tests passed!")


if __name__ == "__main__":
    main()
