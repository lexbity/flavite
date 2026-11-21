"""Selection-first previews, real clipboard effects, timestamps and geometry."""
from datetime import datetime

from PIL import Image, ImageChops

import common
from context_mode import painted


def note(value):
    return next(t for t in common.tabs(value) if t["title"] == "alpha")


def mode(value):
    return value.get("context_mode") or {}


def changed_pixels(first, second, value, region):
    with Image.open(first) as a, Image.open(second) as b:
        sx = a.width / value["window_size"][0]
        sy = a.height / value["window_size"][1]
        x, y, width, height = region
        box = (round(x*sx), round(y*sy), round((x+width)*sx), round((y+height)*sy))
        delta = ImageChops.difference(a.convert("RGB").crop(box), b.convert("RGB").crop(box))
        return sum(max(pixel) > 25 for pixel in delta.getdata())


def geometry(value, names):
    controls = mode(value)["bounds"]
    x, y, width, height = controls["toolbar"]
    rectangles = []
    for name in names:
        rect = controls[name]
        left, top, w, h = rect
        assert left >= x - 0.5 and left+w <= x+width+0.5, (name, rect, controls["toolbar"])
        assert top >= y - 0.5 and top+h <= y+height+0.5, (name, rect)
        rectangles.append((left, left+w, name))
    rectangles.sort()
    for a, b in zip(rectangles, rectangles[1:]):
        assert a[1] <= b[0]+0.5, ("overlapping toolbar controls", a, b)
    assert controls["editor.context_exit"][2] >= 32
    assert controls["editor.context_exit"][3] >= 32


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "alpha" and t["editor_bounds"] for t in common.tabs(s)), "note opened")
    common.click_rect(window, harness, note(value)["editor_bounds"])
    common.palette(harness, "workspace.maximize_group")
    value = wait(lambda s: note(s)["editor_bounds"][2] >= 900, "wide source pane")
    common.key("ctrl+Home")
    common.key("ctrl+space")
    common.key("i")
    value = wait(lambda s: mode(s).get("state") == "TYPING" and note(s)["selections"][note(s).get("primary", 0)] == [0, 0], "typing baseline")
    painted(window, harness, output, "TYPING")
    original = value["focused_text"]
    revision = note(value)["revision"]
    baseline = common.capture(window, output, "01-no-implicit-outline.png", checks)
    common.key("Escape")
    value = wait(lambda s: (mode(s).get("effect") or {}).get("range") == [0, 1], "prospective character range")
    value = painted(window, harness, output, "COMMAND")
    character = common.capture(window, output, "02-character-outline.png", checks)
    x, y, _, _ = note(value)["editor_bounds"]
    # Exclude the caret at x+12..16. Only an actual drawn border around '#'
    # can change these pixels; a harness range by itself cannot pass this.
    assert changed_pixels(baseline, character, value, (x+17, y+8, 10, 26)) >= 12
    assert note(value)["selections"][note(value).get("primary", 0)] == [0, 0]
    assert note(value)["revision"] == revision
    wide = ["editor.context_exit", "state", "editor.cut", "editor.copy", "editor.paste",
            "buffer.undo", "buffer.redo", "workspace.command_palette", "Select", "Go", "Type",
            "workspace.find", "editor.history", "More"]
    value = wait(lambda s: all(name in mode(s).get("bounds", {}) for name in wide) and mode(s)["bounds"]["buffer.undo"][0] > mode(s)["bounds"]["editor.paste"][0], "all wide controls mounted at current geometry")
    geometry(value, wide)

    copy_rect = mode(value)["bounds"]["editor.copy"]
    common.point(window, harness, copy_rect[0]+copy_rect[2]/2, copy_rect[1]+copy_rect[3]/2)
    first_line = original[:original.index("\n")+1]
    value = wait(lambda s: (mode(s).get("effect") or {}).get("command") == "editor.copy", "hover plans the current line")
    assert mode(value)["effect"]["range"] == [0, len(first_line.encode())]
    value = painted(window, harness, output, "Copy outlined line")
    line = common.capture(window, output, "03-line-outline.png", checks)
    assert changed_pixels(character, line, value, (x+28, y+8, 85, 26)) >= 30
    assert note(value)["selections"][note(value).get("primary", 0)] == [0, 0] and note(value)["revision"] == revision
    assert value["focused_text"] == original

    # Copy's effective range never becomes a committed selection. Pasting at
    # the original caret proves the real OS clipboard contains the whole line.
    common.key("i")
    value = wait(lambda s: mode(s).get("state") == "TYPING", "typing retained for toolbar Copy")
    common.click_rect(window, harness, mode(value)["bounds"]["editor.copy"])
    value = wait(lambda s: s.get("status") == "Copied selection" and note(s).get("interaction_focused"), "Copy completed and restored editor focus")
    assert note(value)["selections"][note(value).get("primary", 0)] == [0, 0] and note(value)["revision"] == revision
    assert mode(value)["state"] == "TYPING"
    common.key("ctrl+v")
    wait(lambda s: s.get("focused_text") == first_line+original, "real clipboard line pasted at preserved caret")
    common.key("ctrl+z")
    wait(lambda s: s.get("focused_text") == original and note(s)["selections"][note(s).get("primary", 0)] == [0, 0], "paste undo restores caret")
    common.key("Escape")
    common.key("End")
    common.key("d")
    wait(lambda s: s.get("focused_text") == original[:len(first_line)-2]+original[len(first_line)-1:], "line-end Delete removes the preceding character")
    common.key("u")
    wait(lambda s: s.get("focused_text") == original, "delete undo")
    common.key("shift+d")
    wait(lambda s: s.get("focused_text") == original[len(first_line):], "Cut uses the displayed line rule")
    common.key("u")
    value = wait(lambda s: s.get("focused_text") == original, "cut undo")
    assert note(value)["selections"][note(value).get("primary", 0)] == [len(first_line)-1, len(first_line)-1]

    common.palette(harness, "editor.history")
    value = wait(lambda s: mode(s).get("history", {}).get("open"), "editing history opened")
    entry = next(e for e in mode(value)["history"]["page"]["entries"] if not e["initial"])
    date = datetime.fromtimestamp(entry["changed_at_unix_ms"]/1000).strftime("%Y-%m-%d")
    painted(window, harness, output, date, history=True)
    common.capture(window, output, "04-dated-history.png", checks)
    common.key("Escape")
    wait(lambda s: not mode(s).get("history", {}).get("open"), "history closed")
    common.palette(harness, "workspace.maximize_group")
    common.run("xdotool", "windowsize", "--sync", window, "760", "650")
    value = wait(lambda s: 320 <= mode(s).get("bounds", {}).get("toolbar", [0,0,0,0])[2] < 390, "compact source toolbar")
    geometry(value, ["editor.context_exit", "state", "workspace.command_palette", "More"])
    painted(window, harness, output, "More")
    common.capture(window, output, "05-compact-feedback.png", checks)
    common.click_rect(window, harness, mode(value)["bounds"]["editor.context_exit"])
    wait(lambda s: s.get("context_mode") is None, "Exit stays usable")
    assert (vault/"alpha.md").read_text() == original
    checks.append({"id": "context-effect-paint-clipboard-history-and-layout", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
