#!/usr/bin/env python3
"""Exercise installed production GUI/CLI/helper siblings without the test harness."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

repo = Path(__file__).resolve().parents[3]
output = Path(sys.argv[1]).resolve()
output.mkdir(parents=True, exist_ok=True)
bindir = output / "bin"
bindir.mkdir()
for name in ("flavite", "flavite-cli", "flavite-text-search-worker"):
    shutil.copy2(repo / "target/debug" / name, bindir / name)

def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()

def wait(predicate, label, seconds=30):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if predicate():
            return
        time.sleep(.05)
    raise AssertionError(label)

with tempfile.TemporaryDirectory(prefix="fi-") as temporary:
    root = Path(temporary)
    vault = root / "vault"
    data = root / "d"
    env = {**os.environ, "FLAVITE_DATA_DIR": str(data), "XDG_DATA_HOME": str(root/"d"),
           "XDG_CACHE_HOME": str(root/"cache"), "WINIT_UNIX_BACKEND": "x11"}
    env.pop("WAYLAND_DISPLAY", None)
    env.pop("FLAVITE_HARNESS_OUT_FD", None)
    run(str(bindir / "flavite-cli"), "init", "--path", str(vault), env=env)
    before = b"# Alpha\n\ncat cat\n"
    (vault / "alpha.md").write_bytes(before)
    (vault / "beta.md").write_bytes(b"---\ntitle: Beta\naliases: [Rocket]\n---\n# Beta\n")
    assert "flavite-cli" in run(str(bindir / "flavite-cli"), "--version", env=env)
    assert "--vault" in run(str(bindir / "flavite"), "--help", env=env)
    with (output / "app.log").open("w") as log:
        app = subprocess.Popen([str(bindir / "flavite"), "--vault", str(vault)],
                               env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            window = []
            def find_window():
                found = subprocess.run(["xdotool","search","--onlyvisible","--pid",str(app.pid),
                                        "--name","^Flavite$"], capture_output=True,text=True)
                window[:] = found.stdout.split()
                return bool(window)
            wait(find_window, "installed production window")
            wid = window[0]

            def key(chord):
                run("xdotool","windowfocus","--sync",wid)
                run("xdotool","key","--clearmodifiers",chord)
                time.sleep(.3)

            def type_text(value):
                run("xdotool","type","--clearmodifiers","--delay","60",value)

            def command(title):
                key("ctrl+shift+p")
                type_text(title)
                key("Return")

            def screenshot(name):
                def painted():
                    run("import","-window",wid,str(output/name))
                    return int(run("identify","-format","%k",str(output/name))) > 16
                wait(painted, "installed window painted")

            # The visible frame must exist before OS input; persisted source
            # postconditions below are the semantic oracle for every mutation.
            screenshot("installed-boot.png")
            time.sleep(1)
            key("ctrl+p")
            type_text("alpha")
            key("Return")
            time.sleep(1)
            key("ctrl+f")
            command("Toggle regex matching")
            key("ctrl+f")
            type_text("(cat)")
            time.sleep(1)
            key("ctrl+h")
            type_text("<$1>")
            command("Replace all matches")
            time.sleep(.5)
            key("Escape")
            key("ctrl+s")
            replaced = b"# Alpha\n\n<cat> <cat>\n"
            wait(lambda: (vault/"alpha.md").read_bytes() == replaced, "installed regex helper saved exact source")
            screenshot("installed-regex.png")
            key("ctrl+z")
            key("ctrl+s")
            wait(lambda: (vault/"alpha.md").read_bytes() == before, "installed regex isolated Undo")
            key("ctrl+y")
            key("ctrl+s")
            wait(lambda: (vault/"alpha.md").read_bytes() == replaced, "installed regex Redo")
            key("ctrl+End")
            type_text("[[roc")
            time.sleep(.5)
            screenshot("installed-alias-suggestions.png")
            key("Return")
            time.sleep(.5)
            key("ctrl+s")
            expected = replaced + b"[[beta|Beta]]"
            wait(lambda: (vault/"alpha.md").read_bytes() == expected, "installed alias completion uses canonical target")
            screenshot("installed-completion.png")
            (output/"result.json").write_text(json.dumps({
                "evidence_class":"rendered_os_input","artifact_kind":"installed_production",
                "outcome":"pass","source_sha256":hashlib.sha256(expected).hexdigest(),
                "binary_sha256":{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in bindir.iterdir()},
                "checks":["installed_cli_init_and_version","installed_gui_regex_helper",
                          "one_undo_redo","canonical_alias_completion","exact_saved_bytes"],
            },indent=2)+"\n")
        except BaseException:
            if window:
                screenshot("installed-failure.png")
            shutil.copy2(vault/"alpha.md", output/"actual-alpha.md")
            raise
        finally:
            app.terminate()
            try:
                app.wait(timeout=10)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait()
