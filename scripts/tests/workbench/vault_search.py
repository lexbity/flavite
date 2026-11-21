"""Native source search, exact navigation, reviewed replace, revert and docking."""
import copy
import time
from PIL import Image
import common
from context_mode import _ocr_language


def state(snapshot, owner):
    return next((value for value in snapshot.get("vault_search_states", []) if value["tab"] == owner), {})


def validate_diff(value):
    assert value["diffs"], "sealed diff is missing"
    assert "body" in value["diffs"][0]["before"]
    assert "UPDATED" in value["diffs"][0]["after"]


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    baseline = {path: path.read_bytes() for path in vault.rglob("*.md")}
    common.palette(harness, "workspace.open_search_view")
    value = wait(lambda s: any(tab["load"] == "replace" and tab.get("active") for tab in common.tabs(s)), "source search workbench mounted")
    owner = next(tab["tab"] for tab in common.tabs(value) if tab["load"] == "replace" and tab.get("active"))
    value = wait(lambda s: "query" in state(s, owner).get("bounds", {}), "query input mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["query"])
    common.type_text("body")
    wait(lambda s: state(s, owner).get("query") == "body", "typed source query acknowledged")
    common.key("Return")
    value = wait(lambda s: state(s, owner).get("matches") == 3 and not state(s, owner).get("updating"), "three source matches collected")
    row = state(value, owner)["rows"][0]
    hit = row["contexts"][0]
    name = f"match:{row['ordinal']}:{hit['start']}"
    value = wait(lambda s: name in state(s, owner).get("bounds", {}), "source excerpt mounted")
    common.capture(window, output, "01-source-results.png", checks)
    common.click_rect(window, harness, state(value, owner)["bounds"][name])
    wait(lambda s: s.get("status") == "Opened exact source match" and any(tab.get("selections") == [[hit["start"], hit["end"]]] for tab in common.tabs(s) if tab.get("load") == "ready"), "click opens exact guarded source match")
    common.focus_tab(harness, "Vault search", "replace")
    value = wait(lambda s: "mode-replace" in state(s, owner).get("bounds", {}), "replace row toggle mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["mode-replace"])
    value = wait(lambda s: state(s, owner).get("mode") == "Preview" and "replacement" in state(s, owner).get("bounds", {}), "replacement input mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["replacement"])
    common.type_text("UPDATED")
    wait(lambda s: state(s, owner).get("replacement") == "UPDATED", "typed replacement acknowledged")
    common.key("Return")
    value = wait(lambda s: state(s, owner).get("sealed") and len(state(s, owner).get("candidates", [])) == 3 and not state(s, owner).get("updating"), "replacement preview sealed")
    item = state(value, owner)["candidates"][0]["item"]
    inspect = f"inspect:{item}"
    value = wait(lambda s: inspect in state(s, owner).get("bounds", {}), "inspect control mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"][inspect])
    value = wait(lambda s: bool(state(s, owner).get("diffs")), "sealed readable diff loaded")
    validate_diff(state(value, owner))
    # Actual paint is required in addition to the read model.
    deadline = time.monotonic() + 15
    while True:
        image = common.capture(window, output, "02-reviewed-sealed-diff.png", checks)
        group = next(group for group in value["groups"] if any(tab["tab"] == owner for tab in group["tabs"]))
        x, y, w, h = group["bounds"]
        with Image.open(image) as source:
            sx, sy = source.width/value["window_size"][0], source.height/value["window_size"][1]
            region = source.crop((round(x*sx), round(y*sy), round((x+w)*sx), round((y+h)*sy)))
            region.resize((region.width*2, region.height*2)).save(output / "diff-paint.png")
        ocr = common.run("tesseract", str(output / "diff-paint.png"), "stdout", "-l", _ocr_language, "--psm", "6")
        if "UPDATED" in ocr and "body" in ocr.casefold():
            break
        assert time.monotonic() < deadline, f"readable diff not painted: {ocr}"
        value = common.snapshot(harness)
        time.sleep(0.04)
    assert all(path.read_bytes() == data for path, data in baseline.items()), "preview changed disk"
    negative = copy.deepcopy(state(value, owner))
    negative["diffs"] = []
    try:
        validate_diff(negative)
    except AssertionError:
        checks.append({"id": "missing-readable-diff-negative", "outcome": "pass"})
    else:
        raise AssertionError("missing-diff negative passed")
    value = wait(lambda s: "select-all" in state(s, owner).get("bounds", {}), "note selection control mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["select-all"])
    value = wait(lambda s: state(s, owner).get("selected") == 3 and "apply" in state(s, owner).get("bounds", {}), "reviewed note selection acknowledged")
    common.click_rect(window, harness, state(value, owner)["bounds"]["apply"])
    value = wait(lambda s: (state(s, owner).get("job") or {}).get("saved") == 3, "durable replacement completed")
    assert all(path.read_bytes() == data.replace(b"body", b"UPDATED") for path, data in baseline.items())
    value = wait(lambda s: "revert" in state(s, owner).get("bounds", {}), "guarded Revert mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["revert"])
    wait(lambda s: (state(s, owner).get("job") or {}).get("reverted") == 3, "guarded revert completed")
    assert all(path.read_bytes() == data for path, data in baseline.items())
    value = wait(lambda s: "dock" in state(s, owner).get("bounds", {}), "docking control mounted")
    common.click_rect(window, harness, state(value, owner)["bounds"]["dock"])
    value = wait(lambda s: s.get("sidebar_open") and s.get("vault_search_sidebar") and any(value.get("query") == "body" and value.get("replacement") == "UPDATED" for value in s.get("vault_search_states", [])), "search moved to side panel with state intact")
    common.capture(window, output, "03-docked-vault-search.png", checks)
    checks.append({"id": "vault-search-source-jump-diff-apply-revert-and-docking", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
