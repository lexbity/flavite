"""Context Mode: real native keys, pointer controls, text, undo and paint."""
import common
import re
import time
from PIL import Image

_languages = common.run("tesseract", "--list-langs").splitlines()
_ocr_language = next((language for language in ("eng", "afr") if language in _languages), None)
if _ocr_language is None:
    raise RuntimeError("Context Mode paint checks require a Tesseract Latin language model (eng or afr)")


def painted(window, harness, output, expected, menu=False):
    """Semantic readiness precedes presentation; require the actual control paint."""
    deadline = time.monotonic() + 15
    last = ""
    while time.monotonic() < deadline:
        value = common.snapshot(harness)
        mode = value.get("context_mode") or {}
        rect = mode.get("bounds", {}).get("toolbar")
        if not rect or (menu and expected not in mode.get("bounds", {})):
            time.sleep(0.04)
            continue
        image_path = output / "paint-check.png"
        common.run("import", "-window", window, str(image_path))
        with Image.open(image_path) as img:
            sx = img.width / value["window_size"][0]
            sy = img.height / value["window_size"][1]
            x, y, w, h = rect
            # Include the dropdown card below the bar when it is requested.
            if menu:
                x, y, w, h = mode["bounds"][expected]
            region = img.crop((int(x*sx), int(y*sy), int((x+w)*sx), min(img.height, int((y+h)*sy))))
            region.resize((region.width*2, region.height*2)).save(output / "paint-control.png")
        last = common.run("tesseract", str(output / "paint-control.png"), "stdout", "-l", _ocr_language, "--psm", "6")
        normalize = lambda text: re.sub(r"\W", "", text.casefold())
        if normalize(expected) in normalize(last):
            return value
        time.sleep(0.04)
    raise AssertionError(f"control not painted: {expected!r}; OCR={last!r}")


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    snap = lambda: common.snapshot(harness)
    mode = lambda value: (value.get("context_mode") or {}).get("state")
    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in common.tabs(s)), "source editor ready")
    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.key("ctrl+End")
    common.type_text("red green blue")
    value = wait(lambda s: s.get("focused_text", "").endswith("red green blue"), "ordinary source typing")
    original = value["focused_text"]
    common.key("ctrl+shift+Left")
    common.key("ctrl+space")
    value = wait(lambda s: mode(s) == "COMMAND" and "editor.context_exit" in s["context_mode"]["bounds"], "command toolbar painted")
    bar = value["context_mode"]["bounds"]["toolbar"]
    assert abs(bar[3] - 60.0) < 1.0, bar
    painted(window, harness, output, "COMMAND")
    common.capture(window, output, "01-command-toolbar.png", checks)
    common.key("d")
    value = wait(lambda s: s.get("focused_text", "") != original, "physical delete command")
    assert value["focused_text"].endswith("red green "), value["focused_text"]
    common.key("u")
    wait(lambda s: s.get("focused_text") == original, "shared backend undo")

    common.key("ctrl+End")
    common.key("i")
    wait(lambda s: mode(s) == "TYPING", "temporary typing")
    common.type_text("!")
    common.key("Left")
    wait(lambda s: mode(s) == "TYPING", "reposition retains typing")
    common.type_text("?")
    wait(lambda s: s.get("focused_text", "").endswith("blue?!"), "typing at repositioned caret")
    painted(window, harness, output, "TYPING")
    common.capture(window, output, "02-temporary-typing.png", checks)
    common.key("Escape")
    value = wait(lambda s: mode(s) == "COMMAND", "escape returns to commands")
    stable = value["focused_text"]
    common.key("z")
    value = wait(lambda s: "No command" in s["context_mode"]["message"], "unbound printable key refused")
    assert value["focused_text"] == stable
    common.key("ctrl+a")
    common.key("y")
    painted(window, harness, output, "COMMAND")
    common.key("semicolon")
    common.key("p")
    wait(lambda s: s.get("focused_text") == stable + stable, "native clipboard paste")
    common.key("u")
    wait(lambda s: s.get("focused_text") == stable, "paste is one undo step")

    common.key("slash")
    wait(lambda s: any(t.get("find") for t in common.tabs(s)), "owned Find prompt")
    common.type_text("green")
    value = wait(lambda s: any(t.get("find", {}).get("query") == "green" for t in common.tabs(s) if t.get("find")), "find owns text")
    assert value["focused_text"] == stable
    common.key("Escape")
    wait(lambda s: not any(t.get("find") for t in common.tabs(s)), "Find dismissed")
    common.key("v")
    value = wait(lambda s: (s.get("context_mode") or {}).get("menu") == "Select", "selection dropdown")
    value = painted(window, harness, output, "Select word", menu=True)
    common.capture(window, output, "03-selection-menu.png", checks)
    common.click_rect(window, harness, value["context_mode"]["bounds"]["editor.context_exit"])
    value = wait(lambda s: s.get("context_mode") is None, "Exit works through open dropdown")
    assert value["focused_text"] == stable

    editor = next(t for t in common.tabs(value) if t["title"] == "alpha")
    x, y, width, height = editor["editor_bounds"]
    common.point(window, harness, x + 38, y + 15)
    common.run("xdotool", "click", "3")
    value = wait(lambda s: mode(s) == "COMMAND", "right-click enters mode")
    assert value["focused_text"] == stable
    common.key("ctrl+shift+p")
    wait(lambda s: s.get("palette_open"), "owned palette")
    value = snap()
    common.click_rect(window, harness, value["context_mode"]["bounds"]["editor.context_exit"])
    wait(lambda s: s.get("context_mode") is None and not s.get("palette_open"), "Exit works through owned palette")
    common.key("ctrl+End")
    common.key("ctrl+space")
    wait(lambda s: mode(s) == "COMMAND", "rapid-input source ready")
    common.run("xdotool", "key", "--clearmodifiers", "--delay", "0", "i", "q", "Escape", "d", "u")
    value = wait(lambda s: mode(s) == "COMMAND" and s.get("focused_text") == stable + "q", "queued mode transitions preserve text and commands")

    # The reserved Exit control stays usable when the document pane narrows.
    common.run("xdotool", "windowsize", "--sync", window, "760", "650")
    value = wait(lambda s: s["window_size"][0] <= 780 and (s.get("context_mode") or {}).get("bounds", {}).get("toolbar", [0,0,9999,0])[2] < 430, "compact toolbar after resize")
    value = painted(window, harness, output, "COMMAND")
    common.capture(window, output, "04-compact-toolbar.png", checks)
    common.click_rect(window, harness, value["context_mode"]["bounds"]["More"])
    wait(lambda s: (s.get("context_mode") or {}).get("menu") == "More", "overflow exposes commands")
    value = painted(window, harness, output, "Cut", menu=True)
    common.capture(window, output, "05-compact-menu.png", checks)
    common.click_rect(window, harness, value["context_mode"]["bounds"]["editor.context_exit"])
    value = wait(lambda s: s.get("context_mode") is None, "compact Exit remains clickable through overflow")
    assert value["focused_text"] == stable + "q"
    checks.append({"id": "context-mode-native-input-toolbar-and-exit", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
