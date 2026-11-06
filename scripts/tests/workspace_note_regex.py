#!/usr/bin/env python3
"""S03 native regex replacement check: real OS input plus source receipts."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

window, harness, output, vault = sys.argv[1:]
output, vault = Path(output), Path(vault)
checks = []


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def snapshot():
    for line in reversed(Path(harness).read_text().splitlines()):
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            pass
    return {}


def alpha(value):
    return next(
        (tab for group in value.get("groups", []) for tab in group["tabs"] if tab["title"] == "alpha"),
        {},
    )


def wait(predicate, label, seconds=25):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = snapshot()
        if predicate(value):
            return value
        time.sleep(0.04)
    raise AssertionError(f"{label}: {snapshot()}")


def key(chord):
    run("xdotool", "windowfocus", "--sync", window)
    run("xdotool", "key", "--clearmodifiers", chord)


def type_text(value):
    run("xdotool", "type", "--clearmodifiers", "--delay", "8", value)


def tabs(chord, count):
    for _ in range(count):
        key(chord)


def capture(name):
    path = output / name
    run("import", "-window", window, str(path))
    width, height, colors = map(int, run("identify", "-format", "%w %h %k", str(path)).split())
    assert width >= 640 and height >= 480 and colors >= 16, (width, height, colors)


key("ctrl+p")
type_text("alpha")
key("Return")
wait(lambda state: alpha(state).get("load") == "ready" and alpha(state).get("editor_bounds"), "alpha editor ready")
before = (vault / "alpha.md").read_bytes()
assert before == b"# Alpha\n\ncat scatter cat\n\ncatapult CAT\n", before

key("ctrl+f")
wait(lambda state: alpha(state).get("find") is not None, "find row mounted")
capture("regex-find-literal.png")
# Ctrl+F focuses the Find field. The first tab stop is the visible Regex
# control; activate the same native keyboard action a user can reach.
key("Tab")
key("Return")
wait(lambda state: (alpha(state).get("find") or {}).get("mode") == "regex", "visible regex mode control")
key("ctrl+f")
type_text(r"\b(cat)\b")
state = wait(
    lambda state: (alpha(state).get("find") or {}).get("query") == r"\b(cat)\b"
    and (alpha(state)["find"].get("total_matches") == 3),
    "regex source matches are found by the installed helper",
)
assert alpha(state)["find"]["matches"] == 3
before_find = alpha(state)["buffer_text"]
capture("regex-find-matches.png")
assert alpha(snapshot())["buffer_text"] == before_find, "Find must not edit source"

# Find input -> Regex, Aa, Whole word, Selection only, Previous, Next,
# Replace. Every action is delivered through the native focus ring.
key("ctrl+f")
tabs("Tab", 7)
key("Return")
wait(lambda state: (alpha(state).get("replace") or {}).get("replace_open"), "replacement row mounted")
# The next focus stops are Close Find, then the newly mounted replacement
# field. Typing is therefore still directed through the real text widget.
tabs("Tab", 2)
type_text("<$1>")
state = wait(lambda state: (alpha(state).get("replace") or {}).get("query") == r"\b(cat)\b", "replacement draft retained")
assert alpha(state)["buffer_text"] == before_find
capture("regex-replace-row.png")

# Replacement field -> Replace next -> Replace all.
tabs("Tab", 2)
key("Return")
expected = b"# Alpha\n\n<cat> scatter <cat>\n\ncatapult <CAT>\n"
state = wait(lambda state: alpha(state).get("buffer_text", "").encode() == expected, "one regex Replace all edit")
assert alpha(state)["buffer_text"].encode() == expected
assert alpha(state)["buffer"] is not None
assert (vault / "alpha.md").read_bytes() == before, "Replace all must remain an unsaved buffer edit"

key("ctrl+z")
wait(lambda state: alpha(state).get("buffer_text", "").encode() == before, "one isolated Undo restores original text")
key("ctrl+y")
wait(lambda state: alpha(state).get("buffer_text", "").encode() == expected, "Redo restores one batch")
key("ctrl+s")
wait(lambda state: state["pending_operations"] == 0 and (vault / "alpha.md").read_bytes() == expected, "save writes exact replacement source")
assert hashlib.sha256((vault / "alpha.md").read_bytes()).hexdigest() == hashlib.sha256(expected).hexdigest()

capture("regex-replace.png")
checks.extend(
    [
        {"id": "S03-installed-helper-regex-captures", "outcome": "pass", "artifact": "regex-find-matches.png"},
        {"id": "S03-os-controls-source-unchanged-until-save", "outcome": "pass", "artifact": "regex-replace-row.png"},
        {"id": "S03-replace-all-is-one-undo-unit", "outcome": "pass", "artifact": "regex-replace.png"},
    ]
)
(output / "regex-checks.json").write_text(
    json.dumps({"input_evidence": "rendered_os_input", "checks": checks}, indent=2) + "\n"
)
