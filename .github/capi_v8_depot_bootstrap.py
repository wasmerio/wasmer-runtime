"""Pinned task-local prerequisites, inside the admitted source-phase child."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def output(args, cwd):
    return subprocess.check_output(args, cwd=cwd, text=True, timeout=60).strip()


def prepare(root):
    root = root.resolve(strict=True)
    if root != (Path(os.environ["RUNNER_TEMP"]) / "capi-v8-producer").resolve(strict=True):
        raise RuntimeError("bootstrap root is not this job's admitted recipe")
    owner = json.loads((root / "ownership.json").read_text())
    if owner != {"candidate": os.environ["REQUESTED_CANDIDATE"],
                 "run_id": os.environ["GITHUB_RUN_ID"],
                 "attempt": os.environ["GITHUB_RUN_ATTEMPT"]}:
        raise RuntimeError("bootstrap ownership changed")
    depot, producer = root / "depot_tools", root / "producer"
    source = producer / "v8"
    evidence = root.parent / "capi-v8-evidence"
    identity = json.loads((evidence / "identity.json").read_text())
    if output(["git", "rev-parse", "HEAD"], depot) != identity["depot"]:
        raise RuntimeError("bootstrap depot checkout changed")
    if os.environ.get("DEPOT_TOOLS_UPDATE") != "0" or \
            Path(os.environ["DEPOT_TOOLS_DIR"]).resolve() != depot:
        raise RuntimeError("bootstrap requires the fixed task-local depot")
    for name in ("CUSTOM_CIPD_CLIENT", "VPYTHON_BYPASS", "CHROMIUM_BUILDTOOLS_PATH"):
        if os.environ.get(name):
            raise RuntimeError("bootstrap refuses an alternate tool override: " + name)
    if os.environ.get("DEPOT_TOOLS_BOOTSTRAP_PYTHON3") != "1":
        raise RuntimeError("hermetic Python bootstrap was disabled")

    record = {"depot": identity["depot"], "stage": "bootstrap",
              "source_inputs": {name: digest(depot / name) for name in
                  ("bootstrap_python3", "bootstrap/bootstrap.py", "bootstrap/manifest.txt",
                   "bootstrap/manifest.versions", "python-bin/python3", "gn", "gn.py")},
              "owned_environment": {name: os.environ.get(name) for name in
                  ("DEPOT_TOOLS_DIR", "DEPOT_TOOLS_UPDATE", "DEPOT_TOOLS_BOOTSTRAP_PYTHON3",
                   "TMPDIR", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "CIPD_CACHE_DIR",
                   "VPYTHON_VIRTUALENV_ROOT", "VPYTHON_DOWNLOAD_DIR", "GIT_CONFIG_GLOBAL")}}
    try:
        # This is the same function called by ensure_bootstrap/update_depot_tools.
        # Calling it directly preserves the pinned checkout and limits setup
        # to the Python required by GN.
        subprocess.run(["bash", "-e", "-o", "pipefail", "-c",
            'source "$1/bootstrap_python3"; bootstrap_python3', "bootstrap", str(depot)],
            cwd=depot, check=True, timeout=900)
        relative = (depot / "python3_bin_reldir.txt").read_text().strip()
        directory = (depot / relative).resolve(strict=True)
        if depot not in directory.parents or Path(relative).is_absolute():
            raise RuntimeError("bootstrapped Python is outside the task-local depot")
        python = (directory / "python3").resolve(strict=True)
        if depot not in python.parents or not os.access(python, os.X_OK):
            raise RuntimeError("bootstrapped Python is not a task-local executable")
        wrapper = depot / "python-bin/python3"
        probe = json.loads(output([str(wrapper), "-I", "-c",
            "import json,os,sys; print(json.dumps({'executable': os.path.realpath(sys.executable),"
            "'version': list(sys.version_info[:3]), 'description': sys.version}))"], producer))
        if Path(probe["executable"]) != python or probe["version"] != [3, 11, 8]:
            raise RuntimeError("the pinned manifest's actual Python was not selected")
        record.update(python=probe, python_sha256=digest(python),
                      python_reldir=relative, stage="initial-root-sync")

        # The first sync runs at the actual .gclient root. FindGclientRoot's
        # missing-entries warning is nonfatal; this lets gclient create its own
        # entries before the original script's identical cached subdir sync.
        subprocess.run([str(depot / "gclient"), "sync", "--with_branch_heads",
            "--with_tags", "--nohooks"], cwd=producer, check=True, timeout=5400)
        entries = producer / ".gclient_entries"
        if not entries.is_file() or output([str(depot / "gclient"), "root"], source) != str(producer):
            raise RuntimeError("gclient did not establish the actual solution root")
        if output(["git", "rev-parse", "HEAD"], depot) != identity["depot"] or \
                output(["git", "rev-parse", "HEAD"], source) != identity["v8"]:
            raise RuntimeError("preflight changed an immutable source revision")
        record.update(entries_sha256=digest(entries), stage="gn-launcher")
        selected = output([str(wrapper), "-I", "-c",
            "import sys; sys.path.insert(0, sys.argv[1]); import gn; print(gn.FindGnTool())",
            str(depot)], source)
        gn = Path(selected).resolve(strict=True)
        expected = (source / "buildtools/linux64/gn").resolve(strict=True)
        if gn != expected or source not in gn.parents or not os.access(gn, os.X_OK):
            raise RuntimeError("GN launcher did not select the pinned DEPS executable")
        version = output([str(depot / "gn"), "--version"], source)
        if version != output([str(gn), "--version"], source):
            raise RuntimeError("GN launcher and actual executable disagree")
        record.update(stage="ready", gn={"path": str(gn), "sha256": digest(gn),
                                        "version": version})
    finally:
        (evidence / "bootstrap-preflight.json").write_text(json.dumps(record, indent=2) + "\n")
    return producer


def main(root):
    producer = prepare(root)
    os.chdir(producer)
    # SAME admitted process/session, unchanged original recipe and arguments.
    os.execvpe("bash", ["bash", "candidate-build.sh", "x64", "linux"], os.environ)


if __name__ == "__main__":
    main(Path(sys.argv[1]))
