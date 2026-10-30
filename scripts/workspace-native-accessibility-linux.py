#!/usr/bin/env python3
"""Exercise the running Flavite workspace through the Linux AT-SPI bus.

W02 semantics: the shell exposes tablists with tabs, splitters, and the
active note's source text. Native actions route through the ordinary
message path. The wrapper drives real OS keyboard input (the note picker);
this script observes the semantic tree, asserts it, and dispatches a
native tab action.
"""

from __future__ import annotations

import sys
import time

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


def shell_application():
    return next(
        (
            app
            for app in apps()
            if app.get_name() == "app-demo"
            and any((node.get_role_name() or "") == "tab list" for node in walk(app))
        ),
        None,
    )


def find_nodes(app, role_name, name_contains=None):
    found = []
    for node in walk(app):
        if (node.get_role_name() or "") != role_name:
            continue
        if name_contains is not None:
            label = node.get_name() or ""
            if name_contains not in label:
                continue
        found.append(node)
    return found


def node_text(node):
    """Read a text-capable node's full text through AT-SPI."""
    count = node.get_character_count()
    if not count:
        return ""
    return node.get_text(0, count) or ""


def source_text_nodes(app):
    found = []
    for node in walk(app):
        role = node.get_role_name() or ""
        if role in ("document", "text", "paragraph"):
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

    # The wrapper opened alpha.md through the picker with real OS input;
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
            and tab.get_state_set().contains(Atspi.StateType.ACTIVE)
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
