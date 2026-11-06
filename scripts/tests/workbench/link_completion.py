#!/usr/bin/env python3
"""S05: real keyboard completion, canonical source and isolated Undo."""
import json
from pathlib import Path
import subprocess
import sys
import time

window, harness, output, vault = sys.argv[1:5]
output, vault = Path(output), Path(vault)

def run(*args):
    return subprocess.check_output(args, text=True).strip()

def snapshot():
    for line in reversed(Path(harness).read_text().splitlines()):
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            pass
    return {}

def source(state):
    return next((t for g in state.get("groups", []) for t in g["tabs"] if t["title"] == "alpha"), {})

def wait(predicate, label):
    until = time.monotonic() + 25
    while time.monotonic() < until:
        state = snapshot()
        if predicate(state):
            return state
        time.sleep(.04)
    raise AssertionError(f"{label}: {snapshot()}")

def key(chord):
    run("xdotool", "windowfocus", "--sync", window)
    run("xdotool", "key", "--clearmodifiers", chord)
    time.sleep(.2)

def type_text(value):
    run("xdotool", "type", "--clearmodifiers", "--delay", "45", value)

def capture(name):
    run("import", "-window", window, str(output / name))
    colors = int(run("identify", "-format", "%k", str(output / name)))
    assert colors > 16

key("ctrl+p")
type_text("alpha")
key("Return")
state = wait(lambda s: source(s).get("editor_bounds"), "editor ready")
before = (vault / "alpha.md").read_text()
x, y, w, h = source(state)["editor_bounds"]
run("xdotool", "mousemove", "--window", window, str(int(x+w/2)), str(int(y+h/2)), "click", "1")
key("ctrl+End")
type_text("[[be")
wait(lambda s: source(s).get("buffer_text", "").endswith("[[be"), "captured completion query")
capture("completion-suggestions.png")
key("Return")
state = wait(lambda s: source(s).get("buffer_text", "").endswith("[[beta|Beta]]"), "canonical target from indexed catalog")
assert state["pending_operations"] == 0
capture("completion-inserted.png")
key("ctrl+z")
wait(lambda s: source(s).get("buffer_text", "").endswith("[[be"), "completion is one Undo boundary")
key("ctrl+y")
wait(lambda s: source(s).get("buffer_text", "").endswith("[[beta|Beta]]"), "completion Redo")
key("ctrl+s")
wait(lambda s: (vault / "alpha.md").read_text() == before + "[[beta|Beta]]", "saved canonical bytes")
key("ctrl+End")
type_text("[[roc")
wait(lambda s: source(s).get("buffer_text", "").endswith("[[roc"), "alias completion query")
capture("completion-alias-suggestions.png")
key("Return")
wait(lambda s: source(s).get("buffer_text", "").count("[[beta|Beta]]") == 2, "alias resolves canonical target")
key("ctrl+s")
wait(lambda s: (vault / "alpha.md").read_text() == before + "[[beta|Beta]]" * 2, "alias selection saved canonical source")
(output / "completion-checks.json").write_text(json.dumps({
    "evidence_class": "rendered_os_input",
    "checks": [
        {"id": "S05-completion-keyboard-canonical-source-undo", "outcome": "pass",
         "artifact": "completion-inserted.png"},
    ],
}, indent=2) + "\n")
