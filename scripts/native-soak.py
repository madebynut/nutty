#!/usr/bin/env python3
"""Isolated macOS GUI output/memory probe. Never reads the user's Nutty profile."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid


def process_tree(pid):
    rows = {}
    for line in subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,rss=,pcpu=,comm="], text=True
    ).splitlines():
        parts = line.split(None, 4)
        if len(parts) == 5:
            rows[int(parts[0])] = dict(
                pid=int(parts[0]), parent=int(parts[1]), rss_kib=int(parts[2]),
                cpu=float(parts[3]), executable=parts[4]
            )
    owned = {pid}
    while True:
        expanded = owned | {key for key, row in rows.items() if row["parent"] in owned}
        if owned == expanded:
            return [rows[key] for key in sorted(owned) if key in rows]
        owned = expanded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, default=Path("target/release/Nutty.app"))
    parser.add_argument("--seconds", type=int, default=75)
    parser.add_argument("--panes", type=int, default=8)
    args = parser.parse_args()
    if sys.platform != "darwin" or not 5 <= args.seconds <= 86400 or not 1 <= args.panes <= 64:
        parser.error("macOS required; seconds 5–86400, panes 1–64")
    root = Path(tempfile.mkdtemp(prefix="nutty-native-soak-"))
    bundle = root / "Nutty QA.app"
    shutil.copytree(args.app, bundle)
    plist = bundle / "Contents/Info.plist"
    info = plistlib.loads(plist.read_bytes())
    info["CFBundleIdentifier"] = "dev.madebynut.nutty.qa." + uuid.uuid4().hex
    plist.write_bytes(plistlib.dumps(info))
    subprocess.run(["codesign", "--force", "--sign", "-", str(bundle)], check=True)
    data = root / "data"
    data.mkdir()
    shell = root / "load-shell"
    shell.write_text("#!/bin/sh\n" + shlex.quote(sys.executable) + " -u - <<'PY'\n"
                     "import sys,time\n"
                     "block=('\\033[32mNutty\\033[0m 中文 QA 0123456789 abcdefghijklmnopqrstuvwxyz\\r\\n'*128)\n"
                     "for i in range(600):\n sys.stdout.write(block); sys.stdout.flush(); time.sleep(.05)\n"
                     "print('NUTTY_SOAK_COMPLETE')\nPY\nexec /bin/sh -i\n")
    shell.chmod(0o700)
    (data / "profile.json").write_text(json.dumps(dict(
        schema=1, preferences=dict(shell="/bin/sh", directory=str(root), restore_workspace=True), ai_providers=[]
    )))
    panes, tabs, splits = [], [], []
    for index in range(0, args.panes, 2):
        ids = [str(uuid.uuid4()) for _ in range(min(2, args.panes - index))]
        for pane in ids:
            panes.append(dict(id=pane, source={"Local": dict(shell=str(shell), directory=str(root))},
                              agent=False, shell_open=False, agent_open=True, shell_height=240, agent_height=240))
        branch = ids[0]
        if len(ids) == 2:
            branch = str(uuid.uuid4())
            splits.append(dict(id=branch, axis="Horizontal", first=ids[0], second=ids[1], sizes=[580, 580]))
        tabs.append(dict(id=str(uuid.uuid4()), name=f"QA {index // 2 + 1}", root=branch, active_pane=ids[0]))
    snapshot = dict(version=1, tabs=tabs, panes=panes, splits=splits, active_tab=tabs[0]["id"],
                    sidebar_open=True, sidebar_width=238, tool_width=340)
    (data / "workspace.json").write_text(json.dumps(snapshot))
    samples = []
    with (root / "app.log").open("wb") as log:
        process = subprocess.Popen([str(bundle / "Contents/MacOS/Nutty")],
                                   env={**os.environ, "NUTTY_DATA_DIR": str(data), "RUST_LOG": "warn"},
                                   stdout=log, stderr=log, start_new_session=True)
        try:
            deadline = time.monotonic() + args.seconds
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"QA app exited unexpectedly: {process.returncode}; see {root / 'app.log'}")
                samples.append(dict(time=time.time(), processes=process_tree(process.pid)))
                (root / "memory.json").write_text(json.dumps(samples, indent=2))
                time.sleep(2)
            restored = json.loads((data / "workspace.json").read_text())
            if [pane["id"] for pane in restored["panes"]] != [pane["id"] for pane in panes]:
                raise RuntimeError("Restored pane identities do not match the fixture")
        finally:
            owned = process_tree(process.pid)
            for row in reversed(owned):
                try:
                    os.kill(row["pid"], signal.SIGTERM)
                except ProcessLookupError:
                    pass
            time.sleep(.2)
            for row in owned:
                try:
                    os.kill(row["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait()
    print(json.dumps(dict(evidence=str(root), panes=args.panes, samples=len(samples),
                          gui_peak_kib=max(row["rss_kib"] for sample in samples for row in sample["processes"] if row["pid"] == process.pid),
                          tree_peak_kib=max(sum(row["rss_kib"] for row in sample["processes"]) for sample in samples)), indent=2))


if __name__ == "__main__":
    main()
