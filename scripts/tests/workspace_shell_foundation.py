#!/usr/bin/env python3
"""OS pointer/keyboard proof against the real shell and Session fixture."""
import json
from pathlib import Path
import subprocess
import sys
import time

window, harness, output = sys.argv[1:]
output = Path(output)
checks = []


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def snapshot():
    for line in reversed(Path(harness).read_text().splitlines()):
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue
    return {}


def wait(predicate, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        current = snapshot()
        if predicate(current):
            return current
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {description}; last state: {snapshot()}")


def key(chord):
    run("xdotool", "windowfocus", "--sync", window)
    run("xdotool", "key", "--clearmodifiers", chord)


def move(x, y):
    s = snapshot()
    geometry = dict(line.split("=", 1) for line in run("xdotool", "getwindowgeometry", "--shell", window).splitlines())
    sx = int(geometry["WIDTH"]) / s["window_size"][0]
    sy = int(geometry["HEIGHT"]) / s["window_size"][1]
    run("xdotool", "mousemove", "--sync", "--window", window, str(round(x*sx)), str(round(y*sy)))


def capture(name):
    # Require two stable pixel samples. Semantic readiness precedes GPU
    # presentation, so immediately importing once can capture the old frame.
    previous = None
    deadline = time.monotonic()+5
    while time.monotonic()<deadline:
        run("import", "-window", window, str(output/name))
        pixels = subprocess.check_output(["convert",str(output/name),"RGB:-"])
        if pixels == previous:
            return
        previous = pixels
        time.sleep(0.1)
    raise AssertionError(f"paint did not stabilize for {name}")


def painted_menu(name, before, group_bounds):
    s=snapshot()
    geometry=dict(line.split("=",1) for line in run("xdotool","getwindowgeometry","--shell",window).splitlines())
    sx=int(geometry["WIDTH"])/s["window_size"][0]
    sy=int(geometry["HEIGHT"])/s["window_size"][1]
    x,y,w,_=group_bounds
    crop=f"{round(250*sx)}x{round(210*sy)}+{round((x+w-253)*sx)}+{round((y+36)*sy)}"
    def pixels(path):
        return subprocess.check_output(["convert",str(path),"-crop",crop,"+repage","RGB:-"])
    baseline=pixels(output/before)
    deadline=time.monotonic()+5
    while time.monotonic()<deadline:
        capture(name)
        current=pixels(output/name)
        if len(current)==len(baseline) and sum(a!=b for a,b in zip(current,baseline))>len(current)/4:
            return
        time.sleep(0.05)
    raise AssertionError("menu state was published but the anchored popup did not paint")


def open_note(name):
    key("ctrl+p")
    wait(lambda s: s.get("picker_open"), "picker open")
    run("xdotool", "type", "--delay", "30", name)
    key("Return")
    return wait(lambda s: any(t["title"] == name and t["load"] == "ready" for g in s["groups"] for t in g["tabs"]), "note ready")


def locate(title):
    s = snapshot()
    for g in s["groups"]:
        for t in g["tabs"]:
            if t["title"] == title:
                return g, t
    raise AssertionError(f"missing {title}")


def begin_drag(title):
    g, t = locate(title)
    assert t["visible_in_strip"]
    x, y, _, _ = g["bounds"]
    x += t["slot_index"]*g["tab_width"] + 20
    y += 16
    move(x, y)
    run("xdotool", "mousedown", "1")
    wait(lambda s: s["focused_tab"] == t["tab"], "tab press acknowledged")
    move(x+10, y+8)
    wait(lambda s: s.get("dragging"), "native movement starts tab drag")
    return g, t


open_note("alpha")
wait(lambda s: any(t["editor_bounds"] for g in s["groups"] for t in g["tabs"] if t["title"]=="alpha"), "editor measured")
_, alpha = locate("alpha")
x,y,_,_=alpha["editor_bounds"]
move(x+60,y+20)
run("xdotool","click","1")
wait(lambda s: s["writing_target"] is not None, "real editor caret acknowledged")
run("xdotool","type","--clearmodifiers","--delay","100","dock-marker")
wait(lambda s: "dock-marker" in s["focused_text"], "real input before reparent")
open_note("beta")
wait(lambda s: len(s["groups"]) == 1 and s["groups"][0]["bounds"], "mounted group bounds")
original_group, alpha = locate("alpha")
identity = (alpha["tab"], alpha["buffer"], alpha["revision"], alpha["selection"])
original_tabs = sorted(t["tab"] for t in snapshot()["groups"][0]["tabs"])
# Edge docking must move the selected tab, retaining text/input identity.
begin_drag("alpha")
x, y, w, h = original_group["bounds"]
move(x+w-12, y+h/2)
wait(lambda s: s.get("drag_target") and "Right" in s["drag_target"], "right split target")
capture("02-pointer-split-target.png")
run("xdotool", "mouseup", "1")
wait(lambda s: len(s["groups"]) == 2 and all(g["bounds"] for g in s["groups"]), "edge drop creates split")
g, t = locate("alpha")
assert (t["tab"],t["buffer"],t["revision"],t["selection"]) == identity
assert sorted(t["tab"] for g in snapshot()["groups"] for t in g["tabs"]) == original_tabs
assert snapshot()["writing_target"] and snapshot()["writing_target"]["tab"]==alpha["tab"]
capture("03-pointer-split.png")
checks.append("edge-drop-preserves-tab-buffer-revision-caret")
# Live splitter previews must paint before release and leave desired ratios alone.
s = snapshot()
left = s["groups"][0]["bounds"]
ratios = s["desired_ratios"]
x,y,w,h = left
hit=s["split_hits"][0]
move(hit[0]+hit[2]/2,hit[1]+hit[3]/2)
run("xdotool","mousedown","1")
wait(lambda s:s["resizing"] is not None,"splitter press captured")
move(x+w-90,y+h/2)
wait(lambda s: abs(s["groups"][0]["bounds"][2]-w)>30, "live resize paints before release")
assert snapshot()["desired_ratios"] == ratios
capture("03a-live-splitter-preview.png")
run("xdotool","mouseup","1")
wait(lambda s: s["desired_ratios"]!=ratios, "splitter release commits desired ratio")
checks.append("live-resize-preview-before-single-commit")
# Narrowing temporarily maximizes; restoring size must preserve desired ratios.
ratios = snapshot()["desired_ratios"]
geometry = dict(line.split("=",1) for line in run("xdotool","getwindowgeometry","--shell",window).splitlines())
logical = snapshot()["window_size"]
run("xdotool","windowsize",window,str(round(640*int(geometry["WIDTH"])/logical[0])),str(round(480*int(geometry["HEIGHT"])/logical[1])))
wait(lambda s: s["adaptive_maximized"], "narrow adaptive maximize")
assert snapshot()["desired_ratios"]==ratios
capture("03b-narrow-workspace.png")
run("xdotool","windowsize",window,geometry["WIDTH"],geometry["HEIGHT"])
wait(lambda s: not s["adaptive_maximized"] and abs(s["window_size"][0]-logical[0])<1, "desired arrangement restored")
assert snapshot()["desired_ratios"]==ratios
checks.append("narrow-adaptive-maximize-preserves-desired-ratios")
# Center docking collapses the emptied source group.
source, _ = begin_drag("alpha")
destination = next(g for g in snapshot()["groups"] if g["group"] != source["group"])
x,y,w,h = destination["bounds"]
move(x+w/2, y+h/2)
run("xdotool", "mouseup", "1")
wait(lambda s: len(s["groups"]) == 1, "center drop collapses empty group")
# Reorder through actual strip input.
g, _ = begin_drag("alpha")
x,y,_,_ = g["bounds"]
move(x+3,y+16)
run("xdotool", "mouseup", "1")
wait(lambda s: s["groups"][0]["tabs"][0]["title"] == "alpha", "pointer reorder")
checks.append("center-drop-and-pointer-reorder")
# More opens an anchored overlay; it cannot change panel rectangles.
g = snapshot()["groups"][0]
before = g["bounds"]
x,y,w,h = before
capture("04-before-menu.png")
move(x+w-16,y+16)
run("xdotool", "click", "1")
wait(lambda s: s.get("menu_open"), "More menu")
assert snapshot()["groups"][0]["bounds"] == before
painted_menu("04-anchored-menu.png","04-before-menu.png",before)
key("Escape")
wait(lambda s: not s.get("menu_open"), "Escape dismisses menu")
assert snapshot()["writing_target"] and snapshot()["writing_target"]["tab"]==alpha["tab"]
checks.append("anchored-menu-preserves-content-geometry-and-writing-target")
# Every active tab remains visible, including beyond the old eight-tab cap.
for count in range(12):
    before_count = sum(len(g["tabs"]) for g in snapshot()["groups"])
    key("ctrl+t")
    wait(lambda s: sum(len(g["tabs"]) for g in s["groups"]) == before_count+1, "new overflow tab")
    assert all(t["visible_in_strip"] for g in snapshot()["groups"] for t in g["tabs"] if t["active"])
g = snapshot()["groups"][0]
x,y,w,h = g["bounds"]
move(x+w-80,y+16)
run("xdotool", "click", "1")
wait(lambda s: s.get("menu_open"), "complete tab menu")
capture("05-tab-overflow.png")
key("Home")  # no background editor action is authorized while this menu is open
key("Return")
wait(lambda s: not s.get("menu_open"), "keyboard activates first overflow entry")
assert snapshot()["groups"][0]["tabs"][0]["active"]
checks.append("active-overflow-tab-visible-and-all-tabs-keyboard-reachable")
capture("06-foundation-final.png")
(output/"foundation-checks.json").write_text(json.dumps({"evidence":"rendered_os_input","checks":checks},indent=2)+"\n")
print("shell foundation OS checks passed: " + ", ".join(checks))
