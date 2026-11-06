#!/usr/bin/env python3
"""OS input against the actual graph/editor/filters on the selected backend."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time
from PIL import Image
window, harness, output, vault, backend = sys.argv[1:]
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
def wait(predicate, label):
    end = time.monotonic()+20
    while time.monotonic()<end:
        value = snapshot()
        if predicate(value):
            return value
        time.sleep(.04)
    raise AssertionError(f"{label}: {snapshot()}")
def key(chord):
    run("xdotool","windowfocus","--sync",window)
    run("xdotool","key","--clearmodifiers",chord)
def type_text(value):
    run("xdotool","type","--clearmodifiers","--delay","90",value)
def tabs(value):
    return [t for g in value.get("groups",[]) for t in g["tabs"]]
def graph(value):
    return next((t for t in tabs(value) if t["load"]=="graph" and t["active"]),{})
def point(x,y):
    value=snapshot()
    geometry=dict(row.split("=",1) for row in run("xdotool","getwindowgeometry","--shell",window).splitlines())
    run("xdotool","mousemove","--sync","--window",window,str(round(x*int(geometry["WIDTH"])/value["window_size"][0])),str(round(y*int(geometry["HEIGHT"])/value["window_size"][1])))
def click_rect(rect):
    x,y,w,h=rect
    point(x+w/2,y+h/2)
    run("xdotool","click","1")
def palette(title):
    key("ctrl+shift+p")
    wait(lambda s:s.get("palette_open"),"palette opened")
    key("ctrl+a");type_text(title);key("Return")
    wait(lambda s:not s.get("palette_open"),"palette dispatched")
def focus_graph():
    value=snapshot()
    # The graph tab may have lost the group's active slot when a note was
    # opened (D-03 activation lands in the same document group). Click its
    # strip slot regardless of active state, then require it focused.
    group=next(g for g in value["groups"] if any(t["load"]=="graph" for t in g["tabs"]))
    tab=next(t for t in group["tabs"] if t["load"]=="graph")
    x,y,w,_=group["bounds"]
    point(x+min(w-12,tab["slot_index"]*group["tab_width"]+35),y+16)
    run("xdotool","click","1")
    wait(lambda s:s.get("focused_tab")==tab["tab"],"graph tab focus")
def capture(name):
    path=output/name
    run("import","-window",window,str(path))
    width,height,colors=map(int,run("identify","-format","%w %h %k",str(path)).split())
    assert width>=640 and height>=480 and colors>=16
    checks.append({"id":name,"outcome":"pass","artifact":name})

def ready_graph():
    """The graph snapshot of a *current* frame: readiness first, identity
    renewed (S04 — never retain coordinates across frames)."""
    return wait(lambda s:(g:=graph(s)) and g.get("graph",{}).get("bounds")
                and g["graph"].get("nodes",0)>0 and not g["graph"]["updating"],
                "current graph frame")

def node_by_title(title):
    """Fresh lookup of one node's OS-input center from the newest frame."""
    value=ready_graph()
    return next(n for n in graph(value)["graph"]["screen_nodes"] if n["title"]==title)

def assert_painted_node(node):
    # A semantic hit table alone cannot prove the shader painted at that point.
    # Colored node pixels must occupy the same logical center used by OS input.
    path=output/"graph-selected.png"
    deadline=time.monotonic()+5
    while time.monotonic()<deadline:
        run("import","-window",window,str(path))
        with Image.open(path).convert("RGB") as pixels:
            value=snapshot()
            sx,sy=pixels.width/value["window_size"][0],pixels.height/value["window_size"][1]
            x,y=round(node["x"]*sx),round(node["y"]*sy)
            region=pixels.crop((x-2,y-2,x+3,y+3))
            if any(g>r+15 and b>r+10 and g>90 for r,g,b in (region.getpixel((px,py)) for py in range(region.height) for px in range(region.width))):
                checks.append({"id":"painted-node-matches-os-hit-center","outcome":"pass","artifact":path.name})
                return
        time.sleep(.04)
    raise AssertionError(f"{backend} painted no node at its semantic hit center {node}")

target_before=hashlib.sha256((vault/"beta.md").read_bytes()).hexdigest()
key("ctrl+p");type_text("alpha");key("Return")
value=wait(lambda s:any(t["title"]=="alpha" and t["load"]=="ready" and t.get("editor_bounds") for t in tabs(s)),"source editor mounted")
source=next(t for t in tabs(value) if t["title"]=="alpha")
click_rect(source["editor_bounds"]);key("ctrl+End")
wait(lambda s:s.get("writing_target") is not None,"real caret acknowledgement")
key("ctrl+alt+Right")
wait(lambda s:len(s["groups"])>=2,"real split")
palette("Open graph view")
value=wait(lambda s:graph(s).get("graph",{}).get("nodes",0)>=3 and not graph(s)["graph"]["updating"] and graph(s)["graph"].get("bounds"),"paired graph frame")
assert graph(value)["graph"]["backend"]==backend
palette("Freeze graph layout");palette("Fit graph to view")
value=wait(lambda s:graph(s).get("graph",{}).get("frozen"),"freeze")
# AC-4 (S04/D-02): a selection changes the canvas bounds by exactly zero.
bounds_before=graph(value)["graph"]["bounds"]
node=node_by_title("Beta")
point(node["x"],node["y"]);run("xdotool","click","1")
value=wait(lambda s:graph(s).get("graph",{}).get("selection") is not None,"single-click selection")
assert graph(value)["graph"]["bounds"]==bounds_before, "selection moved the canvas"
assert "[[beta]]" not in next(t for t in tabs(value) if t["tab"]==source["tab"])["buffer_text"]
retained=value["writing_target"]
# Selection emphasis may repaint, but the hit center is renewed from the
# current frame before the painted-pixel proof (S04 retained-coordinate fix).
assert_painted_node(node_by_title("Beta"))
checks.append({"id":"AC-4-selection-zero-bounds","outcome":"pass","artifact":"harness.jsonl"})
# D-02: insertion is the explicit Insert command (Ctrl+Enter), never a
# double click. Each attempt is a new OS gesture; Busy is a zero-mutation
# admission refusal whose displayed repair is exercised with another gesture.
time.sleep(.6)
focus_graph()
for attempt in range(3):
    previous_sequence=value["graph_service"]["publishing"]["sequence"]
    node=node_by_title("Beta")
    point(node["x"],node["y"]);run("xdotool","click","1")
    value=wait(lambda s:graph(s).get("graph",{}).get("selection") is not None,f"selection restored (attempt {attempt})")
    focus_graph()
    key("ctrl+Return")
    value=wait(lambda s:"[[beta]]" in (next(t for t in tabs(s) if t["tab"]==source["tab"]).get("buffer_text") or "") or (s["graph_service"].get("publishing",{}).get("sequence",0)>=previous_sequence+2 and "link.busy" in s.get("status","") and s["pending_operations"]==0),"explicit insertion or Busy refusal")
    current=next(t for t in tabs(value) if t["tab"]==source["tab"])["buffer_text"]
    if "[[beta]]" in current:
        assert current.count("[[beta]]")==1
        break
    assert current==source["buffer_text"], "Busy must not mutate source text"
    checks.append({"id":f"busy-repair-new-os-gesture-{attempt}","outcome":"pass","artifact":"harness.jsonl"})
    time.sleep(.4)
else:
    raise AssertionError("Catalog stayed Busy across three explicit gestures")
assert (value["writing_target"]["buffer"] if "buffer" in value["writing_target"] else value["writing_target"].get("buffer_id")) == (retained["buffer"] if "buffer" in retained else retained.get("buffer_id"))
wait(lambda s:graph(s).get("graph",{}).get("overlay_sources",0)>0 and not graph(s)["graph"]["updating"],"unsaved edge publication")
capture("graph-unsaved.png")
checks.append({"id":"AC-W04-os-explicit-insert-exact-link","outcome":"pass","artifact":"harness.jsonl"})
# D-02: a double click now ACTIVATES the node — it opens the note through
# the shared destination policy and never mutates text. The source graph
# keeps its own group visible, and a user-held camera stays unchanged.
node=node_by_title("Gamma")
point(node["x"],node["y"]);run("xdotool","click","1")
value=wait(lambda s:graph(s).get("graph",{}).get("selection") is not None,"gamma selected")
time.sleep(.6)
node=node_by_title("Gamma")
point(node["x"],node["y"]);run("xdotool","click","--repeat","2","--delay","90","1")
value=wait(lambda s:any(t["title"]=="gamma" and t["load"]=="ready" for t in tabs(s)),"double-click activation opened gamma")
assert "[[gamma]]" not in next(t for t in tabs(value) if t["tab"]==source["tab"])["buffer_text"], "activation mutated the source note"
assert any(t["load"]=="graph" for t in tabs(value)), "the graph view disappeared"
capture("graph-activation-open.png")
checks.append({"id":"AC-4-activation-opens-without-insert","outcome":"pass","artifact":"harness.jsonl"})
focus_graph()
palette("Show graph filters")
value=wait(lambda s:"Path" in graph(s).get("graph",{}).get("filter_bounds",{}),"real filter input mounted")
click_rect(graph(value)["graph"]["filter_bounds"]["Path"]);key("ctrl+a");type_text("missing-folder");key("Return")
value=wait(lambda s:graph(s).get("graph",{}).get("nodes")==0 and not graph(s)["graph"]["updating"],"zero-match filter")
assert graph(value)["graph"]["selection"] is not None and graph(value)["graph"]["outside_scope"]
capture("graph-filtered-empty.png")
palette("Clear graph filters")
wait(lambda s:graph(s).get("graph",{}).get("nodes",0)>=3 and not graph(s)["graph"]["updating"],"filter reversal")
capture("graph-filter-restored.png")
checks.append({"id":"AC-W07-os-filter-reversal-retained-selection","outcome":"pass","artifact":"harness.jsonl"})
value=snapshot();editor=next(t for t in tabs(value) if t["load"]=="ready" and t["active"] and t["buffer"]==retained["buffer"])
click_rect(editor["editor_bounds"]);key("ctrl+z")
wait(lambda s:"[[beta]]" not in (next(t for t in tabs(s) if t["tab"]==source["tab"]).get("buffer_text") or ""),"one undo removes semantic link")
key("ctrl+s")
wait(lambda s:s["pending_operations"]==0 and "[[beta]]" not in (vault/"alpha.md").read_text(),"saved source bytes and terminal completion")
assert hashlib.sha256((vault/"beta.md").read_bytes()).hexdigest()==target_before
assert "[[beta]]" not in (vault/"alpha.md").read_text()
checks.append({"id":"AC-W04-os-undo-target-unchanged","outcome":"pass","artifact":"harness.jsonl"})
(output/"graph-checks.json").write_text(json.dumps({"backend":backend,"input_evidence":"rendered_os_input","checks":checks},indent=2)+"\n")
