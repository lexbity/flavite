"""Reference-driven workspace review: real default layout and OS input."""
import common


def main():
    window, harness, output, vault, backend = common.argv()
    checks = common.checks_store()
    snap = lambda: common.snapshot(harness)
    wait = lambda predicate, label: common.wait(harness, predicate, label)
    tabs = common.tabs
    value = wait(lambda s: len(s.get("groups", [])) == 2 and common.graph(s).get("graph", {}).get("bounds"), "default document/graph workspace")
    graph = common.graph(value)
    graph_id = graph["tab"]
    graph_group = next(g for g in value["groups"] if any(t["tab"] == graph_id for t in g["tabs"]))
    document_group = next(g for g in value["groups"] if g["group"] != graph_group["group"])
    assert document_group["bounds"][0] < graph_group["bounds"][0], "graph must start on the right"
    assert not value["sidebar_open"] and not value["menu_bar_visible"]
    common.capture(window, output, "00-default-workspace.png", checks)

    common.key("ctrl+p")
    common.type_text("alpha")
    common.key("Return")
    value = wait(lambda s: any(t["title"] == "alpha" and t["load"] == "ready" and t["editor_bounds"] for t in tabs(s)), "alpha editor")
    editor = next(t for t in tabs(value) if t["title"] == "alpha")
    common.click_rect(window, harness, editor["editor_bounds"])
    common.key("ctrl+End")
    common.key("Return")
    common.type_text("A note connected to [[beta]].")
    value = wait(lambda s: "A note connected" in s.get("focused_text", ""), "source editing")
    original = value["focused_text"]
    common.capture(window, output, "01-editor-and-graph.png", checks)

    common.key("ctrl+b")
    value = wait(lambda s: s.get("sidebar_open") and s.get("sidebar_bounds", {}).get("filter"), "notes sidebar mounted")
    common.capture(window, output, "02-sidebar.png", checks)
    common.click_rect(window, harness, value["sidebar_bounds"]["filter"])
    common.type_text("beta")
    value = wait(lambda s: s.get("sidebar_filter") == "beta" and s.get("sidebar_bounds", {}).get("note:beta.md"), "sidebar filtered")
    assert next(t for t in tabs(value) if t["tab"] == editor["tab"])["buffer_text"] == original, "sidebar typing edited the document"
    common.capture(window, output, "03-sidebar-filter.png", checks)
    common.click_rect(window, harness, value["sidebar_bounds"]["note:beta.md"])
    value = wait(lambda s: any(t["title"] == "beta" and t["load"] == "ready" and t["active"] for t in tabs(s)), "sidebar opened beta")
    assert common.graph(value)["tab"] == graph_id, "navigation covered the graph"
    common.click_rect(window, harness, value["sidebar_bounds"]["detach"])
    value = wait(lambda s: not s["sidebar_open"] and any(t["load"] == "notes" and t["active"] for t in tabs(s)), "notes opened as tab")
    assert common.graph(value)["tab"] == graph_id
    common.capture(window, output, "04-notes-as-tab.png", checks)
    common.key("ctrl+w")
    wait(lambda s: any(t["title"] == "beta" and t["active"] for t in tabs(s)), "return to note")

    common.point(window, harness, 24, 18)
    common.run("xdotool", "click", "1")
    wait(lambda s: s["menu_bar_visible"], "application menus revealed")
    common.capture(window, output, "04-expanded-menu-bar.png", checks)
    common.point(window, harness, 62, 18)
    common.run("xdotool", "click", "1")
    wait(lambda s: s["menu_open"], "file menu opened")
    common.capture(window, output, "05-file-menu.png", checks)
    common.key("Escape")
    checks.append({"id": "default-layout-sidebar-and-quiet-chrome", "outcome": "pass"})
    common.finish(output, checks, backend)


if __name__ == "__main__":
    main()
