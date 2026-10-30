#!/usr/bin/env python3
"""Exercise the running Flavite shell through the Linux AT-SPI bus."""

from __future__ import annotations

import sys
import time
from pathlib import Path

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


def wait_for(predicate, description, timeout=12.0):
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
            and any(
                node.get_name() == "Editor"
                and "tab" in (node.get_role_name() or "").lower()
                for node in walk(app)
            )
        ),
        None,
    )


def shell_nodes():
    app = shell_application()
    return list(walk(app)) if app is not None else []


def named_role(name, role_fragment):
    return next(
        (
            node
            for node in shell_nodes()
            if node.get_name() == name
            and role_fragment in (node.get_role_name() or "").lower()
        ),
        None,
    )


def invoke(node, action_name):
    action = node.get_action_iface()
    if action is None:
        return False
    for index in range(Atspi.Action.get_n_actions(action)):
        if action_name in (Atspi.Action.get_action_name(action, index) or "").lower():
            if Atspi.Action.do_action(action, index):
                return True
    return False


def main(vault: Path) -> int:
    init_result = Atspi.init()
    try:
        tab = wait_for(
            lambda: named_role("Editor", "tab"),
            "Editor tab in the shell tree",
        )
    except RuntimeError as error:
        discovered = [
            (node.get_role_name(), node.get_name())
            for node in shell_nodes()
        ]
        raise RuntimeError(
            f"{error}; Atspi.init={init_result!r}; discovered={discovered!r}"
        ) from error
    if not invoke(tab, "click"):
        raise RuntimeError("AT-SPI Editor tab did not accept its Click action")

    import subprocess

    windows = subprocess.check_output(
        ["xdotool", "search", "--onlyvisible", "--name", "^Flavite$"], text=True
    ).splitlines()
    if not windows:
        raise RuntimeError("Flavite native window was not found")
    window_id = windows[0]
    subprocess.run(["xdotool", "windowactivate", "--sync", window_id], check=True)
    subprocess.run(["xdotool", "windowfocus", "--sync", window_id], check=True)
    subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+p"], check=True)
    subprocess.run(["xdotool", "type", "note.open"], check=True)
    subprocess.run(["xdotool", "key", "--clearmodifiers", "Return"], check=True)

    original = "# Native bridge note\n\nleft|right\n"

    def text_node_with(expected_text):
        for node in shell_nodes():
            text_iface = node.get_text_iface()
            if text_iface is None:
                continue
            count = Atspi.Text.get_character_count(text_iface)
            if Atspi.Text.get_text(text_iface, 0, count) == expected_text:
                return node
        return None

    editor = wait_for(
        lambda: text_node_with(original),
        "current note text in the editor tree",
    )
    text_iface = editor.get_text_iface()
    count = Atspi.Text.get_character_count(text_iface)
    exposed = Atspi.Text.get_text(text_iface, 0, count)
    if exposed != original:
        raise RuntimeError(f"AT-SPI returned unexpected note text: {exposed!r}")

    component = editor.get_component_iface()
    if component is None or not Atspi.Component.grab_focus(component):
        raise RuntimeError("AT-SPI editor rejected Component.GrabFocus")
    caret = len("# Native bridge note\n\nleft")
    if not Atspi.Text.set_selection(text_iface, 0, caret, caret):
        raise RuntimeError("AT-SPI SetTextSelection action was rejected")

    inserted = "X"
    expected_saved = "# Native bridge note\n\nleftX|right\n"
    subprocess.run(["xdotool", "type", "--delay", "80", inserted], check=True)
    subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+s"], check=True)

    note_path = vault / "demo.md"
    wait_for(
        lambda: note_path.read_text(encoding="utf-8")
        if note_path.exists() and note_path.read_text(encoding="utf-8") == expected_saved
        else None,
        "the caret edit saved to the fixture note",
    )
    print("PASS: AT-SPI read the open note, focused the editor, moved its caret, and saved typed text")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: workspace-native-accessibility-linux.py VAULT_ROOT")
    try:
        raise SystemExit(main(Path(sys.argv[1])))
    except Exception as error:  # A failed native interaction must fail the runner.
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
