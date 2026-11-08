"""Mock-only prerequisite ordering; no tool download, sync or build runs."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import capi_v8_depot_bootstrap as bootstrap


class DepotBootstrapTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="capi-v8-bootstrap-test-")
        self.addCleanup(temporary.cleanup)
        self.temporary = Path(temporary.name).resolve()
        self.root = self.temporary / "capi-v8-producer"
        self.depot = self.root / "depot_tools"
        self.producer = self.root / "producer"
        self.source = self.producer / "v8"
        self.evidence = self.temporary / "capi-v8-evidence"
        self.source.mkdir(parents=True)
        self.evidence.mkdir()
        self.identity = {"depot": "d" * 40, "v8": "a" * 40}
        self.owner = {"candidate": "c" * 40, "run_id": "1", "attempt": "1"}
        (self.root / "ownership.json").write_text(json.dumps(self.owner))
        (self.evidence / "identity.json").write_text(json.dumps(self.identity))
        for name in ("bootstrap_python3", "bootstrap/bootstrap.py", "bootstrap/manifest.txt",
                     "bootstrap/manifest.versions", "python-bin/python3", "gn", "gn.py"):
            path = self.depot / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("mock-only source fixture\n")
        self.python = self.depot / "bootstrap-test/python3/bin/python3"
        self.python.parent.mkdir(parents=True)
        self.python.write_text("not an executable tool; subprocess is mocked\n")
        self.python.chmod(0o700)
        self.gn = self.source / "buildtools/linux64/gn"
        self.gn.parent.mkdir(parents=True)
        self.gn.write_text("not an executable tool; subprocess is mocked\n")
        self.gn.chmod(0o700)
        self.environment = patch.dict(os.environ, {
            "RUNNER_TEMP": str(self.temporary), "REQUESTED_CANDIDATE": self.owner["candidate"],
            "GITHUB_RUN_ID": "1", "GITHUB_RUN_ATTEMPT": "1", "DEPOT_TOOLS_UPDATE": "0",
            "DEPOT_TOOLS_DIR": str(self.depot), "DEPOT_TOOLS_BOOTSTRAP_PYTHON3": "1",
            "CUSTOM_CIPD_CLIENT": "", "VPYTHON_BYPASS": "", "CHROMIUM_BUILDTOOLS_PATH": ""})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def run_tool(self, args, **kwargs):
        if args[0] == "bash":
            (self.depot / "python3_bin_reldir.txt").write_text("bootstrap-test/python3/bin")
        else:
            self.assertEqual(args, [str(self.depot / "gclient"), "sync",
                "--with_branch_heads", "--with_tags", "--nohooks"])
            self.assertEqual(kwargs["cwd"], self.producer)
            (self.producer / ".gclient_entries").write_text("mock-only entries\n")

    def probe(self, args, cwd):
        if args[0] == "git":
            return self.identity["depot" if cwd == self.depot else "v8"]
        if args[0] == str(self.depot / "python-bin/python3"):
            if len(args) == 4:
                return json.dumps({"executable": str(self.python), "version": [3, 11, 8]})
            return str(self.gn)
        if args[-1] == "root":
            return str(self.producer)
        return "mock-only-gn-version"

    def test_pinned_bootstrap_root_sync_and_real_launcher_probe_precede_recipe_exec(self):
        with patch.object(bootstrap.subprocess, "run", side_effect=self.run_tool) as run, \
                patch.object(bootstrap, "output", side_effect=self.probe), \
                patch.object(bootstrap.os, "chdir") as change, \
                patch.object(bootstrap.os, "execvpe") as execute:
            bootstrap.main(self.root)
        self.assertEqual(run.call_args_list[0].args[0], ["bash", "-e", "-o", "pipefail", "-c",
            'source "$1/bootstrap_python3"; bootstrap_python3', "bootstrap", str(self.depot)])
        self.assertEqual(len(run.call_args_list), 2)
        self.assertEqual(json.loads((self.evidence / "bootstrap-preflight.json").read_text())["stage"], "ready")
        change.assert_called_once_with(self.producer)
        self.assertEqual(execute.call_args.args[:2],
            ("bash", ["bash", "candidate-build.sh", "x64", "linux"]))

    def test_failed_bootstrap_never_syncs_or_executes_recipe(self):
        with patch.object(bootstrap.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "bootstrap")) as run, \
                patch.object(bootstrap, "output", side_effect=self.probe), \
                patch.object(bootstrap.os, "execvpe") as execute:
            with self.assertRaises(subprocess.CalledProcessError):
                bootstrap.main(self.root)
        self.assertEqual(run.call_count, 1)
        execute.assert_not_called()
        self.assertEqual(json.loads((self.evidence / "bootstrap-preflight.json").read_text())["stage"], "bootstrap")

    def test_outside_marker_is_rejected_without_sync(self):
        def outside(args, **kwargs):
            (self.depot / "python3_bin_reldir.txt").write_text("../../")
        with patch.object(bootstrap.subprocess, "run", side_effect=outside) as run, \
                patch.object(bootstrap, "output", side_effect=self.probe):
            with self.assertRaisesRegex(RuntimeError, "outside"):
                bootstrap.prepare(self.root)
        self.assertEqual(run.call_count, 1)

    def test_ambient_python_is_rejected_without_sync(self):
        def wrong(args, cwd):
            if args[0] == str(self.depot / "python-bin/python3"):
                return json.dumps({"executable": "/ambient/python3", "version": [3, 11, 8]})
            return self.probe(args, cwd)
        with patch.object(bootstrap.subprocess, "run", side_effect=self.run_tool) as run, \
                patch.object(bootstrap, "output", side_effect=wrong):
            with self.assertRaisesRegex(RuntimeError, "actual Python"):
                bootstrap.prepare(self.root)
        self.assertEqual(run.call_count, 1)

    def test_missing_tool_generated_entries_are_not_fabricated(self):
        def no_entries(args, **kwargs):
            if args[0] == "bash":
                self.run_tool(args, **kwargs)
        with patch.object(bootstrap.subprocess, "run", side_effect=no_entries), \
                patch.object(bootstrap, "output", side_effect=self.probe):
            with self.assertRaisesRegex(RuntimeError, "solution root"):
                bootstrap.prepare(self.root)

    def test_non_deps_gn_is_rejected_before_launcher_execution(self):
        def wrong(args, cwd):
            if len(args) == 5 and args[0] == str(self.depot / "python-bin/python3"):
                return str(self.python)
            return self.probe(args, cwd)
        with patch.object(bootstrap.subprocess, "run", side_effect=self.run_tool), \
                patch.object(bootstrap, "output", side_effect=wrong):
            with self.assertRaisesRegex(RuntimeError, "pinned DEPS"):
                bootstrap.prepare(self.root)


if __name__ == "__main__":
    unittest.main()
