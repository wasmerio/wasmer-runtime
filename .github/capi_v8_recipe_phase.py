"""Admission for the single, pinned upstream wee8 Ninja transition."""

import json
import errno
import os
from pathlib import Path
import time

GIB = 1024**3
SOURCE_CAP = 8 * GIB
BUILD_CAP = 4 * GIB
TOTAL_CAP = 12 * GIB
RESERVE = 2 * GIB

# Replaces itself with the unchanged original recipe only after its parent has
# atomically bound this same session. EOF/cancellation cannot launch the recipe.
START_GATE = """import os, sys
fd = int(sys.argv[1])
ready = os.read(fd, 1)
os.close(fd)
if ready != b'1':
    raise SystemExit('recipe binding was not published')
os.execvpe(sys.argv[2], sys.argv[2:], os.environ)
"""


def available(path):
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize


def atomic_write(path, value):
    temporary = path.with_suffix(".new")
    with temporary.open("x") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
    os.replace(temporary, path)


def load(config):
    path = Path(config["state"])
    if path.is_symlink() or path.parent.resolve() != Path(config["root"]).resolve():
        raise RuntimeError("phase state is not in this private task root")
    state = json.loads(path.read_text())
    for key in ("nonce", "root", "source", "ninja", "source_baseline"):
        if state.get(key) != config[key]:
            raise RuntimeError("phase state does not match the admitted recipe: " + key)
    if state.get("stage") not in ("source", "build"):
        raise RuntimeError("unexpected recipe stage")
    if state["stage"] == "build":
        marker = Path(config["root"]) / "ninja-transition.lock"
        if marker.is_symlink() or json.loads(marker.read_text()) != state:
            raise RuntimeError("build stage lacks its exact one-shot transition record")
    return state


def usage(config, state, free):
    aggregate = max(0, config["source_baseline"] - free)
    if free < RESERVE or aggregate > TOTAL_CAP:
        raise RuntimeError("recipe aggregate growth/reserve exceeded")
    if state["stage"] == "source":
        source_used, build_used = aggregate, 0
        if source_used > SOURCE_CAP:
            raise RuntimeError("source-sync/GN phase exceeded 8 GiB")
    else:
        baseline = state.get("build_baseline")
        if not isinstance(baseline, int) or baseline < 0:
            raise RuntimeError("invalid build baseline")
        source_used = max(0, config["source_baseline"] - baseline)
        build_used = max(0, baseline - free)
        if source_used > SOURCE_CAP or build_used > BUILD_CAP:
            raise RuntimeError("source/build phase exceeded its separate ceiling")
    return {"stage": state["stage"], "source_used": source_used,
            "build_used": build_used, "aggregate_used": aggregate, "free_bytes": free}


def begin(config):
    free = available(config["root"])
    if free < SOURCE_CAP + RESERVE:
        raise RuntimeError("source phase needs 8 GiB growth plus 2 GiB reserve")
    config = dict(config, source_baseline=free)
    state = dict(config, stage="source", recipe_pgid=None)
    path = Path(config["state"])
    if path.parent.resolve() != Path(config["root"]).resolve():
        raise RuntimeError("phase state must be in the private task root")
    if path.exists() or path.is_symlink():
        raise RuntimeError("phase state already exists")
    atomic_write(path, state)
    return config


def bind_process(config, pgid):
    state = load(config)
    if state["stage"] != "source" or state["recipe_pgid"] is not None:
        raise RuntimeError("recipe process was already bound")
    atomic_write(Path(config["state"]), dict(state, recipe_pgid=pgid))


def release_process(config, pgid, ready_fd):
    bind_process(config, pgid)
    if os.write(ready_fd, b"1") != 1:
        raise RuntimeError("recipe startup admission was not delivered")


def close_startup_fds(*descriptors):
    failures = []
    for descriptor in descriptors:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError as error:
                if error.errno != errno.EBADF:
                    failures.append(error)
    return failures


def ninja_entry(config, args):
    # Version queries cannot advance admission. The only allowed inspection is
    # nonexecuting command listing in the original build directory.
    if args == ["--version"]:
        os.execv(config["ninja"], [config["ninja"], "--version"])
        return
    expected_cwd = Path(config["source"]).resolve()
    if Path.cwd().resolve() != expected_cwd:
        raise RuntimeError("Ninja did not run in the expected pinned V8 source")
    if args == ["-C", "out/release", "-t", "commands"]:
        os.execv(config["ninja"], [config["ninja"], *args])
        return
    if args != ["-C", "out/release", "wee8"]:
        raise RuntimeError("unexpected upstream Ninja compile command")
    state = load(config)
    if state["stage"] != "source" or state["recipe_pgid"] != os.getsid(0):
        raise RuntimeError("Ninja is not the admitted original recipe process")
    # A second or concurrent compile transition is refused, never overwritten.
    lock = Path(config["root"]) / "ninja-transition.lock"
    free = available(config["root"])
    usage(config, state, free)
    if free < BUILD_CAP + RESERVE:
        raise RuntimeError("Ninja needs 4 GiB growth plus 2 GiB reserve")
    transitioned = dict(state, stage="build", build_baseline=free,
                        transition_monotonic=time.monotonic(), ninja_pid=os.getpid())
    descriptor = os.open(lock, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        json.dump(transitioned, output)
        output.write("\n")
    atomic_write(Path(config["state"]), transitioned)
    os.execv(config["ninja"], [config["ninja"], "-j2", *args])


def launcher(config):
    """Emit only fixed private paths; no environment-provided continuation."""
    import sys

    module = str(Path(__file__).resolve())
    return ("#!" + sys.executable + "\n"
            "import importlib.util, sys\n"
            f"spec = importlib.util.spec_from_file_location('recipe_phase', {module!r})\n"
            "phase = importlib.util.module_from_spec(spec)\n"
            "spec.loader.exec_module(phase)\n"
            f"phase.ninja_entry({config!r}, sys.argv[1:])\n")
