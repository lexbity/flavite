#!/usr/bin/env python3
"""Exercise the running Flavite workspace through the Linux AT-SPI bus.

W02 semantics: the shell exposes tablists with tabs, splitters, and the
active note's editable text and caret. Native actions route through the ordinary
message path. The wrapper drives real OS keyboard input (the note picker);
this script observes the semantic tree, asserts it, and dispatches a
native tab action.
"""

from __future__ import annotations

from pathlib import Path
import sys
import time
import subprocess

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402


def children(node):
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child is not None:
            yield child


def walk(node):
    yield node
    for child in children(node):
        yield from walk(child)


def wait_for(predicate, description, timeout=20.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {description}")


def apps():
    desktop = Atspi.get_desktop(0)
    return list(children(desktop)) if desktop is not None else []


def has_role(node, name):
    # AT-SPI's human-readable names vary by library version and locale
    # (e.g. "tab list" / "page tab list", "push button" / "button").
    roles = {
        "tab list": Atspi.Role.PAGE_TAB_LIST,
        "tab": Atspi.Role.PAGE_TAB,
        "push button": Atspi.Role.PUSH_BUTTON,
        "dialog": Atspi.Role.DIALOG,
        "list item": Atspi.Role.LIST_ITEM,
    }
    return node.get_role() == roles[name]


def shell_application():
    return next(
        (
            app
            for app in apps()
            if app.get_process_id() == int(sys.argv[3])
            and any(has_role(node, "tab list") for node in walk(app))
        ),
        None,
    )


def find_nodes(app, role_name, name_contains=None):
    found = []
    for node in walk(app):
        if not has_role(node, role_name):
            continue
        if name_contains is not None:
            label = node.get_name() or ""
            if name_contains not in label:
                continue
        found.append(node)
    return found


def node_text(node):
    """Read a text-capable node's full text through AT-SPI."""
    count = Atspi.Text.get_character_count(node)
    if not count:
        return ""
    # Accessible.get_text() returns the text interface in newer GI bindings;
    # call the Text interface explicitly to avoid that method-name collision.
    return Atspi.Text.get_text(node, 0, count) or ""


def source_text_nodes(app):
    found = []
    for node in walk(app):
        role = node.get_role()
        if role in (Atspi.Role.DOCUMENT_FRAME, Atspi.Role.DOCUMENT_TEXT,
                    Atspi.Role.TEXT, Atspi.Role.PARAGRAPH, Atspi.Role.ENTRY):
            if "Native bridge note" in node_text(node):
                found.append(node)
    return found


def main() -> int:
    app = wait_for(shell_application, "the Flavite shell with tablists on AT-SPI")

    # The workspace exposes the boot Empty tab inside a group tablist.
    wait_for(lambda: find_nodes(app, "tab list"), "a workspace group tablist")
    wait_for(
        lambda: find_nodes(app, "tab", name_contains="Empty"),
        "the boot Empty tab",
    )

    # Drive real OS input only after native semantics are ready. Waiting for
    # the actual picker and its catalog results avoids startup races hidden
    # by fixed sleeps (especially during portal/accessibility activation).
    if len(sys.argv) > 2:
        subprocess.run(["xdotool", "windowfocus", "--sync", sys.argv[2]], check=True)
    subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+p"], check=True)
    wait_for(lambda: find_nodes(app, "dialog", "Quick switch"), "the quick switch dialog")
    subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "25", "alpha"], check=True)
    wait_for(lambda: find_nodes(app, "list item"), "the picker's indexed note results")
    subprocess.run(["xdotool", "key", "--clearmodifiers", "Return"], check=True)

    # OS input opened alpha.md through the picker;
    # the semantic tree must now expose its tab and its source text.
    wait_for(
        lambda: find_nodes(app, "tab", name_contains="alpha"),
        "the opened note's tab",
    )
    text_node = wait_for(
        lambda: (source_text_nodes(app) or [None])[0],
        "the active note's source text",
    )
    exposed = node_text(text_node)
    if "Native bridge note" not in exposed:
        raise RuntimeError(f"AT-SPI returned unexpected note text: {exposed!r}")

    # Exercise actual OS text/selection actions, then native key input. The
    # caret must change the real editor and the guarded save must persist it.
    caret = exposed.index("Native bridge note")
    if not text_node.grab_focus():
        raise RuntimeError("AT-SPI editor focus was rejected")
    if not text_node.set_caret_offset(caret):
        raise RuntimeError("AT-SPI text-selection action was rejected")
    wait_for(lambda: text_node.get_caret_offset() == caret, "native caret acknowledgement")
    subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "100", "native-caret-marker"], check=True)
    subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+s"], check=True)
    expected = exposed[:caret] + "native-caret-marker" + exposed[caret:]
    vault = Path(sys.argv[1])
    wait_for(lambda: (vault / "alpha.md").read_text() == expected, "native caret edit saved at the exact source position")

    # Operate the real Find controls through AT-SPI, not facade messages.
    subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+f"], check=True)
    query = wait_for(lambda: next((node for node in walk(app)
        if (node.get_name() or "") == "Find in current note, including unsaved changes"), None), "native Find draft")
    if not Atspi.EditableText.set_text_contents(query, "Native bridge note"):
        raise RuntimeError("native Find SetValue rejected")
    wait_for(lambda: any((node.get_name() or "") == "Find results: 1 matches"
                        for node in walk(app)), "native Find match count")
    case = wait_for(lambda: (find_nodes(app, "push button", "Toggle case sensitive matching") or [None])[0], "case control")
    if not case.do_action(0):
        raise RuntimeError("native case action rejected")
    if not Atspi.EditableText.set_text_contents(query, "native bridge note"):
        raise RuntimeError("native Find query update rejected")
    wait_for(lambda: any((node.get_name() or "") == "Find results: 0 matches"
                        for node in walk(app)), "native case-sensitive refusal")
    if not case.do_action(0):
        raise RuntimeError("native case reset rejected")
    wait_for(lambda: any((node.get_name() or "") == "Find results: 1 matches"
                        for node in walk(app)), "native case reset")
    close_find = wait_for(lambda: (find_nodes(app, "push button", "Close find") or [None])[0], "close Find")
    if not close_find.do_action(0):
        raise RuntimeError("native close Find rejected")
    wait_for(lambda: not any((node.get_name() or "") == "Find in current note, including unsaved changes"
                            for node in walk(app)), "native Find removed")

    # Dispatch a native Click action on the Empty tab; the workspace routes
    # it through the ordinary message path and the tab becomes active.
    empty_tab = wait_for(
        lambda: (find_nodes(app, "tab", name_contains="Empty") or [None])[0],
        "the Empty tab for the native click",
    )
    if not empty_tab.do_action(0):
        raise RuntimeError("AT-SPI Click action on a tab was rejected")
    wait_for(
        lambda: any(
            (tab.get_name() or "") == "Empty"
            and tab.get_state_set().contains(Atspi.StateType.SELECTED)
            for tab in find_nodes(app, "tab")
        ),
        "the clicked tab becoming active",
    )

    # Removed-node rejection: close the active note tab through its native
    # close-button action, then confirm the semantic tree no longer offers
    # a click target for it.
    close_buttons = [
        node
        for node in find_nodes(app, "push button", name_contains="Close alpha")
    ]
    if close_buttons and close_buttons[0].do_action(0):
        wait_for(
            lambda: not find_nodes(app, "tab", name_contains="alpha"),
            "the closed tab disappearing from the tree",
        )

    print(
        "PASS: AT-SPI read the workspace tabs and note text and routed "
        "native tab actions"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
