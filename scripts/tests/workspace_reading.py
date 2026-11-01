#!/usr/bin/env python3
"""Real OS input for W09. Harness data is observation, never reducer injection."""
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
def tabs(value):
    return [tab for group in value.get("groups", []) for tab in group["tabs"]]
def wait(predicate, label):
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        value = snapshot()
        if predicate(value):
            return value
        time.sleep(.04)
    raise AssertionError(f"{label}: {snapshot()}")
def key(chord):
    run("xdotool", "windowfocus", "--sync", window)
    run("xdotool", "key", "--clearmodifiers", chord)
def type_text(value):
    run("xdotool", "type", "--clearmodifiers", "--delay", "8", value)
def click(rect):
    value = snapshot()
    geometry = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", window).splitlines())
    sx, sy = int(geometry["WIDTH"]) / value["window_size"][0], int(geometry["HEIGHT"]) / value["window_size"][1]
    x, y, width, height = rect
    run("xdotool", "mousemove", "--window", window, str(round((x + min(width / 2, 100)) * sx)), str(round((y + height / 2) * sy)))
    run("xdotool", "click", "1")
def palette(label):
    key("ctrl+shift+p")
    wait(lambda s: s.get("palette_open"), "palette mounted")
    type_text(label)
    value = wait(lambda s: s.get("palette_query") == label and s.get("palette_matches"), "palette query acknowledged")
    expected = {"Toggle note Edit / Read":"workspace.note_mode", "Open Outline":"workspace.open_outline", "Open Properties":"workspace.open_properties", "Open preview of the followed note":"workspace.open_preview", "Toggle source / rendered Preview":"context.source_preview"}[label]
    assert value["palette_matches"][0] == expected, (label, value["palette_matches"])
    key("Return")
def source(value):
    return next((t for t in tabs(value) if t["title"] == "alpha"), {})
def active(value):
    return next((t for t in tabs(value) if t["tab"] == value.get("focused_tab")), {})
def control(tab, role, first=True):
    controls = [c for c in (tab.get("reading") or {}).get("controls", []) if c["role"] == role]
    return (controls[0] if first and controls else controls[-1] if controls else None)
def capture(name, expected):
    path = output / name
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        run("import", "-window", window, str(path))
        width, height, colors = map(int, run("identify", "-format", "%w %h %k", str(path)).split())
        assert width >= 640 and height >= 480 and colors >= 16
        # Semantic readiness precedes the compositor's presented frame. Verify
        # actual glyphs in the captured pixels as well as mounted control data.
        recognized = run("tesseract", str(path), "stdout", "--psm", "11").casefold()
        if all(word.casefold() in recognized for word in expected):
            break
        time.sleep(.04)
    else:
        raise AssertionError(f"{name}: rendered content missing {expected}: {recognized}")
    checks.append({"id": name, "outcome": "pass", "artifact": name})

beta_before = hashlib.sha256((vault / "beta.md").read_bytes()).hexdigest()
key("ctrl+p"); type_text("alpha"); key("Return")
value = wait(lambda s: source(s).get("editor_bounds") and source(s).get("load") == "ready", "source editor")
source_id, buffer_id = source(value)["tab"], source(value)["buffer"]
click(source(value)["editor_bounds"]); key("ctrl+End")
wait(lambda s: source(s).get("selection") == [len(source(s).get("buffer_text", ""))] * 2, "document-end caret acknowledged")
type_text("\nDirty reading canary")
wait(lambda s: "Dirty reading canary" in source(s).get("buffer_text", ""), "real committed edit")
key("ctrl+e")
value = wait(lambda s: source(s).get("mode") == "Read" and (source(s).get("reading") or {}).get("buffer") == [buffer_id, source(s).get("revision")] and control(source(s), "task"), "rendered current-buffer revision and actual task control")
assert source(value)["tab"] == source_id and source(value)["buffer"] == buffer_id
assert value["writing_target"] is None
capture("reading-dirty.png", ["Column A", "First task", "bold"])
# Click the real task widget, using its observed mounted bounds.
click(control(source(value), "task")["bounds"])
wait(lambda s: "[x] First task" in source(s).get("buffer_text", "") and s["pending_operations"] == 0, "guarded task completion")
key("ctrl+e")
value = wait(lambda s: source(s).get("mode") == "Edit", "same tab returns to editor")
click(source(value)["editor_bounds"]); key("ctrl+z")
value = wait(lambda s: "[ ] First task" in source(s).get("buffer_text", ""), "one Undo removes only task change")
assert "Dirty reading canary" in source(value)["buffer_text"]
key("ctrl+y")
wait(lambda s: "[x] First task" in source(s).get("buffer_text", ""), "task Redo")
# Dirty in-note find uses the live buffer rather than the saved search index.
before_find = source(snapshot())["buffer_text"]
key("ctrl+f"); wait(lambda s: source(s).get("find") is not None, "find field mounted"); type_text("Dirty reading canary")
value = wait(lambda s: (source(s).get("find") or {}).get("query") == "Dirty reading canary" and source(s)["find"]["matches"] == 1, "dirty find")
assert source(value)["buffer_text"] == before_find, "find typing must not edit the note"
capture("reading-dirty-find.png", ["Dirty reading canary", "Find"])
# Close Find by keyboard through the palette; mode switching preserves source.
palette("Toggle note Edit / Read")
wait(lambda s: source(s).get("mode") == "Read" and source(s).get("reading"), "read after find")
capture("reading-find-highlight.png", ["Dirty reading canary", "beta body", "after"])

palette("Open Outline")
value = wait(lambda s: active(s).get("load") == "outline" and control(active(s), "outline"), "bound outline controls")
assert active(value)["effective_source"] == "alpha.md"
assert any(h["title"] == "More detail" for h in active(value)["reading"]["outline"])
capture("reading-outline.png", ["More detail"])
heading = control(active(value), "outline", False)
click(heading["bounds"])
value = wait(lambda s: s.get("focused_tab") == source_id and source(s).get("reading") and any(h["byte"] == heading["byte"] and h.get("bounds") for h in source(s)["reading"]["outline"]), "outline activates its source and mounts the selected heading")
# Near the end, scrolling clamps to the viewport's bottom. The selected
# heading must remain in view; its byte need not become the first visible byte.
selected = next(h for h in source(value)["reading"]["outline"] if h["byte"] == heading["byte"])
_, heading_y, _, heading_height = selected["bounds"]
_, viewport_y, _, viewport_height = source(value)["editor_bounds"]
visible_y = heading_y - source(value)["reading"]["scroll_y"]
assert visible_y + heading_height > viewport_y and visible_y < viewport_y + viewport_height, "selected heading must be visible"


palette("Open Properties")
value = wait(lambda s: active(s).get("load") == "properties" and control(active(s), "property-input") and control(active(s), "property-apply"), "typed property controls")
assert active(value)["effective_source"] == "alpha.md"
# title is the first scalar in canonical source order.
click(control(active(value), "property-input")["bounds"]); key("ctrl+a"); type_text("Revised Alpha")
value = snapshot(); click(control(active(value), "property-apply")["bounds"])
wait(lambda s: "title: 'Revised Alpha' # preserve this comment" in source(s).get("buffer_text", "") and s["pending_operations"] == 0, "byte-preserving typed property patch")
capture("reading-properties.png", ["Revised Alpha", "count"])
palette("Open preview of the followed note")
value = wait(lambda s: active(s).get("load") == "preview" and (active(s).get("reading") or {}).get("buffer") == [buffer_id, source(s).get("revision")], "rendered preview follows current source revision")
assert active(value)["effective_source"] == "alpha.md"
capture("reading-preview.png", ["Column A", "First task", "bold"])
palette("Toggle source / rendered Preview")
wait(lambda s: active(s).get("source_preview"), "explicit Source preview")
capture("reading-preview-source.png", ["preserve this comment", "let literal"])
# Save through the source tab and verify actual fixture bytes and target safety.
key("ctrl+p"); type_text("alpha"); key("Return")
wait(lambda s: s.get("focused_tab") == source_id, "source canonical routing")
key("ctrl+s")
wait(lambda s: s["pending_operations"] == 0 and "Revised Alpha" in (vault / "alpha.md").read_text(), "terminal source save")
assert "[x] First task" in (vault / "alpha.md").read_text()
assert "Dirty reading canary" in (vault / "alpha.md").read_text()
assert hashlib.sha256((vault / "beta.md").read_bytes()).hexdigest() == beta_before
value = snapshot(); ratios = value["desired_ratios"]
run("xdotool", "windowsize", window, "640", "480")
wait(lambda s: s["window_size"] == [640, 480], "640x480 read view")
value = snapshot()
x, y, width, height = source(value)["editor_bounds"]
run("xdotool", "mousemove", "--window", window, str(round(x + width / 2)), str(round(y + height / 2)))
run("xdotool", "click", "--repeat", "20", "--delay", "20", "4")
wait(lambda s: (source(s).get("reading") or {}).get("scroll_y") == 0, "narrow source scrolled to top")
capture("reading-narrow.png", ["Frontmatter", "Revised Alpha"])
assert snapshot()["desired_ratios"] == ratios
checks.extend([{"id": "AC-W08-os-mode-task-undo-preview", "outcome": "pass", "artifact": "harness.jsonl"}, {"id": "W09-os-outline-properties-dirty-find", "outcome": "pass", "artifact": "harness.jsonl"}, {"id": "W09-saved-source-target-unchanged", "outcome": "pass", "artifact": "harness.jsonl"}])
(output / "reading-checks.json").write_text(json.dumps({"input_evidence": "rendered_os_input", "checks": checks}, indent=2) + "\n")
