"""Mock-only guards: no Ninja, compiler, source fetch or recipe is executed."""

import json
import errno
from pathlib import Path
import tempfile
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import capi_v8_recipe_phase as phase


class RecipePhaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="capi-v8-phase-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.config = {"root": str(self.root), "source": str(self.source),
                       "state": str(self.root / "state.json"), "ninja": "/never-executed/ninja",
                       "nonce": "only-this-mocked-recipe"}

    def space(self, count):
        return patch.object(phase.os, "statvfs", return_value=SimpleNamespace(
            f_bavail=count // 4096, f_frsize=4096))

    def begin(self, free=14 * phase.GIB):
        with self.space(free):
            self.config = phase.begin(self.config)
        phase.bind_process(self.config, 12345)

    def transition(self, free=8 * phase.GIB, args=None):
        args = ["-C", "out/release", "wee8"] if args is None else args
        with self.space(free), patch.object(phase.Path, "cwd", return_value=self.source), \
                patch.object(phase.os, "getsid", return_value=12345), \
                patch.object(phase.os, "execv") as execute:
            phase.ninja_entry(self.config, args)
            return execute.call_args

    def test_source_admission_uses_eight_plus_reserve_without_lowering_total(self):
        self.begin(10 * phase.GIB)
        self.assertEqual(phase.TOTAL_CAP, 12 * phase.GIB)
        with self.space(10 * phase.GIB - 4096):
            other = dict(self.config, state=str(self.root / "other.json"))
            with self.assertRaisesRegex(RuntimeError, "8 GiB"):
                phase.begin(other)

    def test_exact_compile_transition_records_baseline_before_exec(self):
        self.begin()
        called = self.transition(7 * phase.GIB)
        state = phase.load(self.config)
        self.assertEqual(state["stage"], "build")
        self.assertEqual(state["build_baseline"], 7 * phase.GIB)
        self.assertEqual(json.loads((self.root / "ninja-transition.lock").read_text()), state)
        self.assertEqual(called.args, ("/never-executed/ninja",
            ["/never-executed/ninja", "-j2", "-C", "out/release", "wee8"]))

    def test_startup_byte_is_sent_only_after_the_exact_session_is_bound(self):
        with self.space(14 * phase.GIB):
            self.config = phase.begin(self.config)

        def send(fd, value):
            self.assertEqual(phase.load(self.config)["recipe_pgid"], 12345)
            self.assertEqual((fd, value), (99, b"1"))
            return 1

        with patch.object(phase.os, "write", side_effect=send) as sent:
            phase.release_process(self.config, 12345, 99)
            sent.assert_called_once()

    def test_child_startup_gate_never_executes_on_parent_eof(self):
        with patch.object(sys, "argv", ["gate", "99", "bash", "candidate-build.sh", "x64", "linux"]), \
                patch.object(phase.os, "read", return_value=b""), \
                patch.object(phase.os, "close"), patch.object(phase.os, "execvpe") as execute:
            with self.assertRaisesRegex(SystemExit, "not published"):
                exec(phase.START_GATE, {})
            execute.assert_not_called()
        with patch.object(sys, "argv", ["gate", "99", "bash", "candidate-build.sh", "x64", "linux"]), \
                patch.object(phase.os, "read", return_value=b"1"), \
                patch.object(phase.os, "close"), patch.object(phase.os, "execvpe") as execute:
            exec(phase.START_GATE, {})
            self.assertEqual(execute.call_args.args[:2],
                             ("bash", ["bash", "candidate-build.sh", "x64", "linux"]))

    def test_closed_startup_fd_cannot_interrupt_remaining_cleanup(self):
        with patch.object(phase.os, "close", side_effect=[OSError(errno.EBADF, "closed"), None]) as close:
            self.assertEqual(phase.close_startup_fds(11, None, 12), [])
            self.assertEqual(close.call_count, 2)
        error = OSError(errno.EIO, "other failure")
        with patch.object(phase.os, "close", side_effect=[error, None]) as close:
            self.assertEqual(phase.close_startup_fds(11, 12), [error])
            self.assertEqual(close.call_count, 2)

    def test_version_and_command_inspection_do_not_transition(self):
        self.begin()
        for args in (["--version"], ["-C", "out/release", "-t", "commands"]):
            with patch.object(phase.Path, "cwd", return_value=self.source), \
                    patch.object(phase.os, "execv") as execute:
                phase.ninja_entry(self.config, args)
                execute.assert_called_once()
            self.assertEqual(phase.load(self.config)["stage"], "source")
            self.assertFalse((self.root / "ninja-transition.lock").exists())

    def test_generated_launcher_uses_the_same_mocked_entry_without_environment_paths(self):
        self.begin()
        text = phase.launcher(self.config)
        with patch.object(sys, "argv", ["ninja", "--version"]), \
                patch.object(phase.os, "execv") as execute:
            exec(compile(text, "<owned-ninja-launcher>", "exec"), {})
            execute.assert_called_once_with("/never-executed/ninja",
                                            ["/never-executed/ninja", "--version"])
        self.assertEqual(phase.load(self.config)["stage"], "source")

    def test_other_targets_flags_and_mutating_tools_refuse_before_exec(self):
        self.begin()
        for args in (["-C", "out/release", "v8_monolith"],
                     ["-C", "out/release", "wee8", "-j8"],
                     ["-C", "out/release", "-t", "clean"], ["wee8"]):
            with patch.object(phase.Path, "cwd", return_value=self.source), \
                    patch.object(phase.os, "execv") as execute:
                with self.assertRaisesRegex(RuntimeError, "unexpected"):
                    phase.ninja_entry(self.config, args)
                execute.assert_not_called()
        self.assertEqual(phase.load(self.config)["stage"], "source")

    def test_wrong_cwd_or_recipe_session_cannot_transition(self):
        self.begin()
        with patch.object(phase.Path, "cwd", return_value=self.root), \
                patch.object(phase.os, "execv") as execute:
            with self.assertRaisesRegex(RuntimeError, "expected"):
                phase.ninja_entry(self.config, ["-C", "out/release", "wee8"])
            execute.assert_not_called()
        with patch.object(phase.Path, "cwd", return_value=self.source), \
                patch.object(phase.os, "getsid", return_value=999), \
                patch.object(phase.os, "execv") as execute:
            with self.assertRaisesRegex(RuntimeError, "recipe process"):
                phase.ninja_entry(self.config, ["-C", "out/release", "wee8"])
            execute.assert_not_called()

    def test_source_and_build_admission_caps_refuse_before_exec(self):
        self.begin(15 * phase.GIB)
        # 9 GiB was consumed before the transition, although aggregate <=12.
        with self.assertRaisesRegex(RuntimeError, "source-sync"):
            self.transition(6 * phase.GIB)
        # Another root has source <=8 but insufficient current 4+2 free.
        self.config["source_baseline"] = 13 * phase.GIB
        state = phase.load(dict(self.config, source_baseline=15 * phase.GIB))
        state["source_baseline"] = self.config["source_baseline"]
        phase.atomic_write(Path(self.config["state"]), state)
        with self.assertRaisesRegex(RuntimeError, "4 GiB"):
            self.transition(6 * phase.GIB - 4096)

    def test_phase_and_aggregate_checks_include_final_sample(self):
        self.begin(15 * phase.GIB)
        source = phase.load(self.config)
        with self.assertRaisesRegex(RuntimeError, "source-sync"):
            phase.usage(self.config, source, 6 * phase.GIB)
        self.transition(7 * phase.GIB)
        built = phase.load(self.config)
        phase.usage(self.config, built, 3 * phase.GIB)  # Exactly 8+4 used.
        with self.assertRaisesRegex(RuntimeError, "aggregate"):
            phase.usage(self.config, built, 3 * phase.GIB - 4096)
        with self.assertRaisesRegex(RuntimeError, "reserve"):
            phase.usage(self.config, built, phase.RESERVE - 4096)

    def test_build_cap_can_fail_while_total_is_below_twelve(self):
        self.begin(14 * phase.GIB)
        self.transition(10 * phase.GIB)
        state = phase.load(self.config)
        with self.assertRaisesRegex(RuntimeError, "separate ceiling"):
            phase.usage(self.config, state, 6 * phase.GIB - 4096)

    def test_forged_state_identity_or_build_without_marker_refuses(self):
        self.begin()
        state = phase.load(self.config)
        state["nonce"] = "another-job"
        phase.atomic_write(Path(self.config["state"]), state)
        with self.assertRaisesRegex(RuntimeError, "nonce"):
            phase.load(self.config)
        state["nonce"] = self.config["nonce"]
        state.update(stage="build", build_baseline=9 * phase.GIB)
        phase.atomic_write(Path(self.config["state"]), state)
        with self.assertRaises((OSError, RuntimeError)):
            phase.load(self.config)

    def test_repeated_transition_is_not_a_continuation(self):
        self.begin()
        self.transition()
        with self.assertRaisesRegex(RuntimeError, "recipe process"):
            self.transition()


if __name__ == "__main__":
    unittest.main()
