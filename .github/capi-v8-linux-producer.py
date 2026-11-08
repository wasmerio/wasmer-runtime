"""Private packaging experiment; this does not certify the Wasmer C API.

The upstream recipe, its eight existing patches and GN arguments stay fixed.
Only the upstream library-TLS and reviewed canonical-reference patches change V8 source. A task-local depot path
and a two-job Ninja launcher replace the recipe's shared /tmp and unbounded jobs.
"""

import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import tarfile
import time

import capi_v8_recipe_phase as recipe_phase


PRODUCER = "844d01dc10edaa0461715f484e06b004f1fd023e"
V8 = "b0a55a7dad7f536cce1f9aaddba89894c8533946"
DEPOT = "289a37bda3d848cfe16e6c1c599741ed3890ab55"
TLS_SOURCE = "775f97e372aad1fb6ad119883ae177213c1773a8"
TLS_PATCH_SHA = "ac7f245e80788ac2b32a5453216d37f2ad165d6a598fac943d723a063cbfc119"
BUILD_SCRIPT_SHA = "8c0140c953f0eae74453facc5083cb209470c3f818833c9dd40411b772077875"
HEADERS_SHA = "9eb4e6bfa1f9a849f82bdeb7c485646621631e9ed1136c34b052c2ef641cfb41"
DEPS_SHA = "75967c8cdbf3d5c999a050ca2556019693a43067e0e31c68a8693be0f7dc94c7"
REF_PATCH_NAME = "0010-Canonicalize-callable-references-at-C-API-boundaries.patch"
REF_PATCH_SHA = "d51568aa5e8e782515fe92c22ec1c9e2f9d9be926ebeaab9effbf0b41596088d"
REF_CC_SHA = "8fe3e7e08f3488743b8a386ae434427d4e87996a4ef6a50705e0a4d118d3e822"
REF_HH_SHA = "67eae9b38696c2620e797e3c96c30441ea5dfdfa62957d1f635040c010429729"
C_HEADER_SHA = "97f49de5418f02c55633dbcdc96b72fb404659c9275e9a47a3b4d6bdce384b96"
GIB = 1024**3


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def checked_text(args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True, timeout=30).strip()


def main():
    temporary = Path(os.environ["RUNNER_TEMP"]).resolve()
    root = temporary / "capi-v8-producer"
    evidence = temporary / "capi-v8-evidence"
    evidence.mkdir(mode=0o700, exist_ok=False)
    root.mkdir(mode=0o700, exist_ok=False)
    (root / "ownership.json").write_text(json.dumps({
        "run_id": os.environ["GITHUB_RUN_ID"], "attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "candidate": os.environ["REQUESTED_CANDIDATE"],
    }))
    candidate = Path(os.environ["GITHUB_WORKSPACE"]) / "candidate"
    if platform.machine() != "x86_64" or platform.system() != "Linux":
        raise RuntimeError("the reviewed candidate requires Linux x86_64")
    if checked_text(["git", "rev-parse", "HEAD"], candidate) != os.environ["REQUESTED_CANDIDATE"]:
        raise RuntimeError("candidate identity changed")
    subprocess.run(["git", "diff", "HEAD", "--exit-code"], cwd=candidate, check=True)
    if checked_text(["git", "status", "--porcelain", "--untracked-files=all"], candidate):
        raise RuntimeError("candidate has tracked or untracked source changes")
    for tool in ("git", "cc", "c++", "gcc", "g++", "ninja", "readelf", "nm", "ldd", "curl"):
        if not shutil.which(tool):
            raise RuntimeError("required preinstalled tool is absent: " + tool)
    env = os.environ.copy()
    # depot_tools otherwise accepts an inherited shared Git-cache destination.
    env.pop("GIT_CACHE_PATH", None)
    for name, relative in {
        "TMPDIR": "tmp", "XDG_CACHE_HOME": "cache", "XDG_CONFIG_HOME": "config",
        "CLOUDSDK_CONFIG": "cloud-config", "CIPD_CACHE_DIR": "cipd-cache",
        "VPYTHON_VIRTUALENV_ROOT": "vpython", "VPYTHON_DOWNLOAD_DIR": "vpython-downloads",
        "GIT_CONFIG_GLOBAL": "gitconfig",
    }.items():
        path = root / relative
        if name == "GIT_CONFIG_GLOBAL":
            path.touch()
        else:
            path.mkdir()
        env[name] = str(path)
    # Keep the runner's HOME identity unchanged; configure tool-specific state.
    (root / "boto-config").touch()
    env.update(DEPOT_TOOLS_UPDATE="0", DEPOT_TOOLS_METRICS="0", V8_TAG=V8,
               GIT_CONFIG_NOSYSTEM="1", BOTO_CONFIG=str(root / "boto-config"),
               BOTO_PATH=str(root / "boto-config"))
    deadline = time.monotonic() + 250 * 60
    measurements = []

    def run(name, args, cwd, growth, seconds):
        initial = shutil.disk_usage(root).free
        phase_config = None
        if name == "upstream-recipe":
            phase_config = recipe_phase.begin({
                "root": str(root), "state": str(root / "recipe-state.json"),
                "source": str(source.resolve()), "ninja": str(ninja),
                "nonce": os.urandom(16).hex(),
            })
            initial = phase_config["source_baseline"]
            (launchers / "ninja").write_text(recipe_phase.launcher(phase_config))
            (launchers / "ninja").chmod(0o700)
        elif initial < growth + 2 * GIB:
            raise RuntimeError(f"{name}: free={initial}, growth ceiling={growth}, reserve={2 * GIB}")
        started = time.monotonic()
        child = None
        status = None
        ready_read = ready_write = None
        try:
            with (evidence / (name + ".log")).open("wb") as log:
                command = args
                pass_fds = ()
                if phase_config is not None:
                    ready_read, ready_write = os.pipe()
                    pass_fds = (ready_read,)
                    command = [sys.executable, "-c", recipe_phase.START_GATE, str(ready_read), *args]
                child = subprocess.Popen(command, cwd=cwd, env=env, stdout=log,
                    stderr=subprocess.STDOUT, start_new_session=True, pass_fds=pass_fds)
                if phase_config is not None:
                    os.close(ready_read)
                    ready_read = None
                    recipe_phase.release_process(phase_config, child.pid, ready_write)
                    os.close(ready_write)
                    ready_write = None
                while True:
                    try:
                        status = child.wait(timeout=10)
                        break
                    except subprocess.TimeoutExpired:
                        free = shutil.disk_usage(root).free
                        if phase_config is not None:
                            state = recipe_phase.load(phase_config)
                            sample = recipe_phase.usage(phase_config, state, free)
                            (evidence / "recipe-current.json").write_text(json.dumps(sample, indent=2) + "\n")
                        if (time.monotonic() >= min(deadline, started + seconds)
                                or free < 2 * GIB or initial - free > growth):
                            raise RuntimeError(name + ": deadline or measured disk guard exceeded")
                if status:
                    raise RuntimeError(f"{name} exited {status}; exact log retained")
        finally:
            primary_active = sys.exc_info()[0] is not None
            fd_failures = recipe_phase.close_startup_fds(ready_read, ready_write)
            if child is not None:
                # Retire this command's process group, including a surviving descendant.
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                if child.poll() is None:
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait(timeout=5)
            final = shutil.disk_usage(root).free
            measurements.append(dict(phase=name, command=args, cwd=str(cwd), exit=status,
                                     seconds=time.monotonic() - started, initial_free=initial,
                                     final_free=final, measured_growth=max(0, initial - final),
                                     growth_ceiling=growth, reserve=2 * GIB))
            (evidence / "phases.json").write_text(json.dumps(measurements, indent=2) + "\n")
            if fd_failures:
                (evidence / "startup-fd-errors.txt").write_text("\n".join(map(str, fd_failures)) + "\n")
                if not primary_active:
                    raise fd_failures[0]
            if phase_config is not None:
                try:
                    state = recipe_phase.load(phase_config)
                    shutil.copyfile(root / "recipe-state.json", evidence / "recipe-state.json")
                    if state["stage"] == "build":
                        shutil.copyfile(root / "ninja-transition.lock", evidence / "ninja-transition.json")
                    sample = recipe_phase.usage(phase_config, state, final)
                    sample.update(source_cap=recipe_phase.SOURCE_CAP,
                                  build_cap=recipe_phase.BUILD_CAP,
                                  aggregate_cap=recipe_phase.TOTAL_CAP, reserve=recipe_phase.RESERVE)
                    (evidence / "recipe-final.json").write_text(json.dumps(sample, indent=2) + "\n")
                    if status == 0 and state["stage"] != "build":
                        raise RuntimeError("recipe returned without its admitted Ninja transition")
                except Exception as error:
                    (evidence / "recipe-guard-error.txt").write_text(str(error) + "\n")
                    if not primary_active:
                        raise
            if final < 2 * GIB or initial - final > growth:
                # Record every failure, but retain an already-active primary error.
                if sys.exc_info()[0] is None:
                    raise RuntimeError(name + ": final measured disk guard exceeded")

    def clone(name, url, revision):
        dest = root / name
        run(name + "-init", ["git", "init", str(dest)], root, GIB, 30)
        run(name + "-remote", ["git", "remote", "add", "origin", url], dest, GIB, 30)
        run(name + "-fetch", ["git", "fetch", "--depth=1", "origin", revision], dest, 3 * GIB, 300)
        run(name + "-checkout", ["git", "checkout", "--detach", "FETCH_HEAD"], dest, 3 * GIB, 120)
        if checked_text(["git", "rev-parse", "HEAD"], dest) != revision:
            raise RuntimeError(name + ": immutable checkout mismatch")
        return dest

    producer = clone("producer", "https://github.com/wasmerio/v8-custom-builds.git", PRODUCER)
    depot = clone("depot_tools", "https://chromium.googlesource.com/chromium/tools/depot_tools.git", DEPOT)
    env.update(DEPOT_TOOLS_DIR=str(depot), DEPOT_TOOLS_BOOTSTRAP_PYTHON3="1")
    for override in ("CUSTOM_CIPD_CLIENT", "VPYTHON_BYPASS", "CHROMIUM_BUILDTOOLS_PATH"):
        env.pop(override, None)
    source = clone("source-v8", "https://chromium.googlesource.com/v8/v8.git", V8)
    source.rename(producer / "v8")
    source = producer / "v8"
    if digest(source / "DEPS") != DEPS_SHA:
        raise RuntimeError("immutable V8 DEPS checksum mismatch")
    (producer / ".gclient").write_text(
        "solutions = [{\"name\": \"v8\", \"url\": \"https://chromium.googlesource.com/v8/v8.git\", "
        "\"deps_file\": \"DEPS\", \"managed\": False, \"custom_deps\": {}}]\n")
    # These are task-local tool wrappers, not changes to V8/GN/DEPS.
    launchers = root / "bin"
    launchers.mkdir()
    ninja = Path(shutil.which("ninja")).resolve()
    # The exact phased launcher is installed at source admission, before any
    # original recipe command. Version/command inspections never advance it.
    env["PATH"] = str(launchers) + os.pathsep + str(depot) + os.pathsep + env["PATH"]
    tls = producer / "patches/0009-Use-library-safe-TLS-on-Linux.patch"
    run("fetch-authoritative-tls-patch", ["curl", "--fail", "--location", "--proto", "=https",
        "--tlsv1.2", "--connect-timeout", "15", "--max-time", "60", "--output", str(tls),
        f"https://raw.githubusercontent.com/wasmerio/v8-custom-builds/{TLS_SOURCE}/patches/{tls.name}"],
        producer, GIB, 90)
    if digest(tls) != TLS_PATCH_SHA:
        raise RuntimeError("authoritative TLS patch checksum mismatch")
    reference_input = candidate / "lib/api/third-party/wee8/canonical-references.patch"
    if digest(reference_input) != REF_PATCH_SHA:
        raise RuntimeError("reviewed canonical-reference patch checksum mismatch")
    references = producer / "patches" / REF_PATCH_NAME
    shutil.copyfile(reference_input, references)
    if digest(references) != REF_PATCH_SHA:
        raise RuntimeError("canonical-reference patch copy changed")
    shutil.copyfile(references, evidence / REF_PATCH_NAME)
    original = (producer / "build.sh").read_bytes()
    if hashlib.sha256(original).hexdigest() != BUILD_SCRIPT_SHA:
        raise RuntimeError("original producer recipe checksum mismatch")
    replacement = b'DEPOT_TOOLS_DIR="/tmp/depot_tools"'
    if original.count(replacement) != 1:
        raise RuntimeError("original task-local adaptation site changed")
    recipe = original.replace(replacement, ('DEPOT_TOOLS_DIR="' + str(depot) + '"').encode())
    (producer / "candidate-build.sh").write_bytes(recipe)
    (evidence / "original-build.sh").write_bytes(original)
    (evidence / "task-local-build.sh").write_bytes(recipe)
    shutil.copyfile(tls, evidence / tls.name)
    tools = {name: checked_text([name, "--version"]) for name in ("cc", "c++", "gcc", "g++", "ninja", "readelf", "git", "ldd")}
    patches = {p.name: digest(p) for p in sorted((producer / "patches").glob("*.patch"))}
    if len(patches) != 10 or list(patches)[-2:] != [tls.name, REF_PATCH_NAME]:
        raise RuntimeError("expected original eight patches, TLS9, then reviewed canonical refs10")
    identity = dict(candidate=os.environ["REQUESTED_CANDIDATE"], producer=PRODUCER, v8=V8,
                    depot=DEPOT, tls_source=TLS_SOURCE, tls_sha256=TLS_PATCH_SHA,
                    canonical_refs_patch_sha256=REF_PATCH_SHA,
                    candidate_wasm_hh_sha256=REF_HH_SHA, unchanged_wasm_h_sha256=C_HEADER_SHA,
                    original_build_sha256=BUILD_SCRIPT_SHA, original_compiler="unrecorded; no ELF .comment",
                    original_runner="ubuntu-22.04; system GCC and depot_tools were unpinned",
                    compiler_equivalence="not established; no bit-reproducibility claim", tools=tools,
                    host=platform.platform(), deps_sha256=DEPS_SHA, patches_sha256=patches,
                    invocation_environment={key: env.get(key) for key in
                        ("CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS", "LD_LIBRARY_PATH")},
                    lock_sha256=digest(candidate / "Cargo.lock"), jobs=2,
                    phase_module_sha256=digest(Path(recipe_phase.__file__)),
                    bootstrap_module_sha256=digest(Path(__file__).with_name("capi_v8_depot_bootstrap.py")),
                    source_growth_cap=8 * GIB, build_growth_cap=4 * GIB,
                    aggregate_growth_cap=12 * GIB, reserve=2 * GIB,
                    phase_estimates="new guarded estimates, not historical measured peaks; GN may invoke compiler checks")
    (evidence / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    # Bootstrap/preflight is inside the SAME admitted source phase and deadline.
    # It execs the unchanged original recipe, which applies the eight patches, TLS9, then reviewed refs10 with plain git apply.
    run("upstream-recipe", [sys.executable, str(Path(__file__).with_name("capi_v8_depot_bootstrap.py")),
        str(root)], producer, 12 * GIB, 230 * 60)
    if checked_text(["git", "rev-parse", "HEAD"], source) != V8:
        raise RuntimeError("V8 source moved")
    expected_sources = {
        "src/wasm/c-api.cc": REF_CC_SHA,
        "third_party/wasm-api/wasm.hh": REF_HH_SHA,
        "third_party/wasm-api/wasm.h": C_HEADER_SHA,
    }
    for relative, expected_sha in expected_sources.items():
        if digest(source / relative) != expected_sha:
            raise RuntimeError("canonical candidate source/header mismatch: " + relative)
    shutil.copyfile(source / "third_party/wasm-api/wasm.hh", evidence / "candidate-wasm.hh")
    (evidence / "canonical-ref-sources.json").write_text(
        json.dumps(expected_sources, indent=2) + "\n")
    shutil.copyfile(source / "DEPS", evidence / "DEPS")
    shutil.copyfile(source / "out/release/args.gn", evidence / "args.gn")
    run("actual-dependencies", [str(depot / "gclient"), "revinfo", "--actual"], producer, GIB, 120)
    run("actual-compile-commands", [str(ninja), "-C", str(source / "out/release"), "-t", "commands"], producer, GIB, 120)
    gn = (source / "buildtools/linux64/gn").resolve(strict=True)
    if root not in gn.parents:
        raise RuntimeError("GN is not from this task's pinned DEPS")
    (evidence / "gn.json").write_text(json.dumps({
        "path": str(gn), "sha256": digest(gn),
        "version": checked_text([str(gn), "--version"]),
        "deps_git_revision": "6e8e0d6d4a151ab2ed9b4a35366e630c55888444",
    }, indent=2) + "\n")
    run("source-diff", ["git", "diff", "--binary", "HEAD"], source, GIB, 60)
    dist = source / "out/dist"
    headers = sorted([[p.relative_to(dist).as_posix(), digest(p)]
                      for p in (dist / "include").rglob("*") if p.is_file()])
    manifest = json.dumps(headers, separators=(",", ":")).encode()
    (evidence / "headers.json").write_bytes(manifest + b"\n")
    if hashlib.sha256(manifest).hexdigest() != HEADERS_SHA:
        raise RuntimeError("published header set differs from the original archive")
    archive = dist / "lib/libv8.a"
    run("relocations", ["readelf", "-rW", str(archive)], producer, GIB, 120)
    relocations = (evidence / "relocations.log").read_text(errors="replace")
    if "R_X86_64_TPOFF32" in relocations or "R_AARCH64_TLSLE" in relocations:
        raise RuntimeError("candidate still has forbidden local-exec TLS")
    run("undefined-symbols", ["nm", "-u", str(archive)], producer, GIB, 120)
    if "__isoc23_" in (evidence / "undefined-symbols.log").read_text(errors="replace"):
        raise RuntimeError("candidate requires glibc 2.38 C23 symbols")
    # Standard CAPI compiler-driver shared linkage, with the C++ library declared
    # by lib/api/build.rs. The wrapper calls only the existing wee8 C API.
    smoke = root / "smoke.c"
    smoke.write_text('''#include "wasm.h"
int candidate_engine_store_smoke(void) {
    wasm_engine_t *e = wasm_engine_new();
    if (!e) return 1;
    wasm_store_t *s = wasm_store_new(e);
    if (!s) { wasm_engine_delete(e); return 2; }
    wasm_store_delete(s);
    wasm_engine_delete(e);
    return 0;
}
''')
    shutil.copyfile(smoke, evidence / "smoke.c")
    shared = root / "libcandidate-wee8.so"
    run("shared-link", ["cc", "-shared", "-fPIC", "-I", str(dist / "include/wasm-c-api"),
        str(smoke), str(archive), "-lstdc++", "-o", str(shared)], root, GIB, 120)
    # Run dlopen out of process so fatal V8 startup preserves this evidence writer.
    smoke_script = "import ctypes,os; m=ctypes.CDLL(os.environ['CANDIDATE_SHARED'],mode=os.RTLD_NOW); f=m.candidate_engine_store_smoke; f.restype=ctypes.c_int; assert f()==0; print('actual wee8 engine/store create/delete PASS')"
    env["CANDIDATE_SHARED"] = str(shared)
    run("dlopen-smoke", [sys.executable, "-c", smoke_script], root, GIB, 60)
    run("shared-abi", ["readelf", "-dVW", str(shared)], root, GIB, 60)
    run("shared-symbols", ["nm", "-D", str(shared)], root, GIB, 60)
    initial = shutil.disk_usage(root).free
    if initial < 3 * GIB:
        raise RuntimeError("packaging requires a 1 GiB ceiling plus 2 GiB reserve")
    started = time.monotonic()
    try:
        shutil.copyfile(smoke, evidence / "smoke.c")
        shutil.copyfile(shared, evidence / shared.name)
        shutil.copyfile(producer / "LICENSE", evidence / "producer-LICENSE")
        shutil.copyfile(source / "LICENSE", evidence / "V8-LICENSE")
        with tarfile.open(evidence / "UNCERTIFIED-v8-linux-amd64.tar.xz", "w:xz") as output:
            output.add(dist / "include", arcname="include")
            output.add(dist / "lib", arcname="lib")
            output.add(source / "LICENSE", arcname="LICENSE")
    finally:
        final = shutil.disk_usage(root).free
        measurements.append(dict(phase="packaging", seconds=time.monotonic() - started,
            initial_free=initial, final_free=final, measured_growth=max(0, initial - final),
            growth_ceiling=GIB, reserve=2 * GIB))
        (evidence / "phases.json").write_text(json.dumps(measurements, indent=2) + "\n")
        if sys.exc_info()[0] is None and (final < 2 * GIB or initial - final > GIB):
            raise RuntimeError("packaging exceeded its actual disk guard")
    (evidence / "result.json").write_text(json.dumps({"packaging_smoke": "passed", "capi_wasi_acceptance": False,
        "archive_sha256": digest(evidence / "UNCERTIFIED-v8-linux-amd64.tar.xz"),
        "library_sha256": digest(archive), "headers_sha256": HEADERS_SHA}, indent=2) + "\n")


if __name__ == "__main__":
    def interrupted(number, _frame):
        raise InterruptedError("runner cancellation signal " + str(number))

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        main()
    finally:
        evidence = Path(os.environ["RUNNER_TEMP"]) / "capi-v8-evidence"
        if evidence.is_dir():
            hashes = {p.relative_to(evidence).as_posix(): digest(p) for p in sorted(evidence.rglob("*"))
                      if p.is_file() and p.name != "checksums.json"}
            (evidence / "checksums.json").write_text(json.dumps(hashes, indent=2) + "\n")
