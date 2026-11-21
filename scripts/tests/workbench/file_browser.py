"""Real directory navigation and docking, preserving a dirty document."""
import common
from PIL import Image
from context_mode import _ocr_language


def validate(value):
    rows = value["file_browser"]["sidebar_rows"]
    assert any(row["path"] == "Systems" and row["expanded"] for row in rows)
    assert any(row["path"] == "Systems/gamma.md" and row["depth"] == 1 for row in rows)


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "alpha" and t.get("editor_bounds") for t in common.tabs(s)), "source ready")
    note = next(t for t in common.tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, note["editor_bounds"])
    common.key("ctrl+End")
    common.type_text(" unsaved")
    wait(lambda s: s.get("focused_text", "").endswith("unsaved"), "dirty source preserved")
    common.key("ctrl+b")
    value = wait(lambda s: s.get("sidebar_open") and "folder:Systems" in s.get("sidebar_bounds", {}), "real directory mounted")
    common.click_rect(window, harness, value["sidebar_bounds"]["folder:Systems"])
    value = wait(lambda s: any(row["path"] == "Systems/gamma.md" for row in s.get("file_browser", {}).get("sidebar_rows", [])) and "note:Systems/gamma.md" in s.get("sidebar_bounds", {}), "folder expands to actual child")
    validate(value)
    expanded_snapshot = value
    image = common.capture(window, output, "01-expanded-vault-tree.png", checks)
    # Inspect the actual folder paint at its mounted bounds, avoiding
    # unrelated graph/editor text in OCR's line segmentation.
    x, y, w, h = value["sidebar_bounds"]["folder:Systems"]
    with Image.open(image) as source:
        sx, sy = source.width/value["window_size"][0], source.height/value["window_size"][1]
        region = source.crop((round(x*sx), round(y*sy), round((x+w)*sx), round((y+h)*sy)))
        region.resize((region.width*3, region.height*3)).save(output / "folder-paint.png")
    ocr = common.run("tesseract", str(output / "folder-paint.png"), "stdout", "-l", _ocr_language, "--psm", "7").casefold()
    assert "systems" in ocr, ocr
    # Keyboard and pointer share the same open path. One Down, one Enter.
    common.key("Down")
    wait(lambda s: any(view.get("tab") is None and view.get("selected") == "Systems/gamma.md" for view in s["file_browser"]["views"]), "keyboard selects child")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "gamma" and t.get("load") == "ready" for t in common.tabs(s)), "keyboard opens nested document")
    assert any(t["title"] == "alpha" and t.get("buffer_text", "").endswith("unsaved") for t in common.tabs(value))
    assert (vault / "alpha.md").read_text() == "# Alpha\n\nalpha body\n"
    value = wait(lambda s: "detach" in s.get("sidebar_bounds", {}), "detach control mounted")
    common.click_rect(window, harness, value["sidebar_bounds"]["detach"])
    value = wait(lambda s: not s.get("sidebar_open") and any(t.get("load") == "notes" for t in common.tabs(s)), "browser moved to workspace tab")
    browser_tab = next(t["tab"] for t in common.tabs(value) if t.get("load") == "notes")
    assert any(view.get("tab") == browser_tab and "Systems" in view["expanded"] for view in value["file_browser"]["views"])
    common.capture(window, output, "02-browser-workspace-tab.png", checks)
    # Seeded validator negative: deleting the expanded child must fail.
    import copy
    negative = copy.deepcopy(expanded_snapshot)
    negative["file_browser"]["sidebar_rows"] = [row for row in negative["file_browser"]["sidebar_rows"] if row["path"] != "Systems/gamma.md"]
    try:
        validate(negative)
    except AssertionError:
        checks.append({"id": "missing-child-negative", "outcome": "pass"})
    else:
        raise AssertionError("missing-child negative passed")
    checks.append({"id": "file-browser-pointer-keyboard-dirty-safety-and-docking", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
