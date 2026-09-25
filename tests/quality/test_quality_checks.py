"""Tests for the gates themselves, including deliberate bad inputs."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


complexity = load("check-complexity")
coverage = load("coverage-summary")


class ComplexityTests(unittest.TestCase):
    def test_existing_hotspot_cannot_grow(self):
        baseline = complexity.baseline_for({"src/a.rs::open": [28]})
        self.assertEqual(complexity.check({"src/a.rs::open": [28]}, baseline), [])
        self.assertTrue(complexity.check({"src/a.rs::open": [29]}, baseline))

    def test_new_functions_have_a_ceiling(self):
        baseline = complexity.baseline_for({"src/a.rs::old": [1]})
        self.assertEqual(complexity.check({"src/a.rs::new": [15]}, baseline), [])
        self.assertTrue(complexity.check({"src/a.rs::new": [16]}, baseline))

    def test_same_named_methods_do_not_share_one_allowance(self):
        baseline = complexity.baseline_for({"src/a.rs::open": [28, 10]})
        self.assertTrue(complexity.check({"src/a.rs::open": [28, 20]}, baseline))

    def test_reductions_must_be_locked_in(self):
        baseline = complexity.baseline_for({"src/a.rs::open": [28]})
        self.assertTrue(complexity.check({"src/a.rs::open": [20]}, baseline))
        self.assertTrue(complexity.check({}, baseline))
        reduced = complexity.baseline_for({"src/a.rs::open": [20]})
        self.assertEqual(complexity.check({"src/a.rs::open": [20]}, reduced), [])

    def test_bad_schema_is_not_a_pass(self):
        with self.assertRaises(ValueError):
            complexity.check({}, {"schema": 42})

    def test_rust_lifetimes_and_duplicate_methods_are_measured(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src/example.rs").write_text("""
impl<'a> A<'a> { fn open(&self) { if true {} } }
impl B { fn open(&self) { if true {} if false {} } }
fn follow<'a>(x: &'a str) -> &'a str { if x.is_empty() { return x; } x }
fn last() { }
""")
            measured = complexity.measure(root)
            self.assertEqual(measured["src/example.rs::open"], [3, 2])
            self.assertEqual(measured["src/example.rs::follow"], [2])
            self.assertEqual(measured["src/example.rs::last"], [1])

    def test_rust_char_literals_do_not_hide_following_lifetime_functions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src/example.rs").write_text(r"""
fn char_cases<'a>(input: &'a str) -> &'a str {
    let c = 'c';
    let r = 'r';
    let apostrophe = '\'';
    let backslash = '\\';
    let newline = '\n';
    if c == 'c' { return input; }
    if r == 'r' { return input; }
    if apostrophe == '\'' { return input; }
    if backslash == '\\' { return input; }
    if newline == '\n' { return input; }
    input
}
fn following<'a>(input: &'a str) -> &'a str { input }
""")
            measured = complexity.measure(root)
            self.assertEqual(measured["src/example.rs::char_cases"], [6])
            self.assertEqual(measured["src/example.rs::following"], [1])


def coverage_fixture():
    return {"type": "llvm.coverage.json.export", "data": [{"files": [
        {"filename": "/workspace/src/" + name, "summary": {
            "branches": {"count": 10, "covered": 4},
            "lines": {"count": 20, "covered": 15},
        }} for name in coverage.FILES
    ]}]}


class CoverageTests(unittest.TestCase):
    def test_summary_uses_counts_not_untrusted_percent(self):
        report = coverage_fixture()
        report["data"][0]["files"][0]["summary"]["branches"]["percent"] = 100
        self.assertIn("4/10 (40.0%)", coverage.summarize(report))

    def test_stable_zero_branches_cannot_look_like_success(self):
        report = coverage_fixture()
        report["data"][0]["files"][0]["summary"]["branches"] = {"count": 0, "covered": 0}
        with self.assertRaises(ValueError):
            coverage.summarize(report)

    def test_missing_or_duplicate_sources_fail(self):
        for duplicate in (False, True):
            report = coverage_fixture()
            files = report["data"][0]["files"]
            if duplicate:
                files.append(files[0])
            else:
                files.pop()
            with self.assertRaises(ValueError):
                coverage.summarize(report)

    def test_impossible_counts_fail(self):
        report = coverage_fixture()
        report["data"][0]["files"][0]["summary"]["branches"]["covered"] = 11
        with self.assertRaises(ValueError):
            coverage.summarize(report)


class IsolationTests(unittest.TestCase):
    def run_isolated(self, *args):
        return subprocess.run([str(ROOT / "scripts/with-test-home.sh"), *args],
                              env={**os.environ, "SECRET_SENTINEL": "must-not-leak",
                                   "PIKA_DB_PATH": "/not/a/test/database",
                                   "LLVM_PROFILE_FILE": "/tmp/quality-%p.profraw"},
                              text=True, capture_output=True)

    def test_state_is_disposable_and_coverage_environment_survives(self):
        result = self.run_isolated("/usr/bin/env")
        self.assertEqual(result.returncode, 0, result.stderr)
        env = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
        self.assertNotIn("SECRET_SENTINEL", env)
        self.assertTrue(env["PIKA_DB_PATH"].startswith("/tmp/pq."))
        self.assertFalse(Path(env["HOME"]).exists(), "temporary HOME not cleaned")
        self.assertEqual(env["LLVM_PROFILE_FILE"], "/tmp/quality-%p.profraw")
        self.assertNotIn("TMUX", env)

    def test_failure_remains_failure(self):
        self.assertEqual(self.run_isolated("/bin/sh", "-c", "exit 23").returncode, 23)

    def test_real_provider_and_ssh_commands_are_blocked(self):
        for command in ("codex", "claude", "opencode", "muse", "ssh", "tailscale"):
            result = self.run_isolated(command, "--version")
            self.assertEqual(result.returncode, 97, result.stderr)


class RunnerTests(unittest.TestCase):
    def fixture(self, directory, runner, cargo_source):
        root = Path(directory)
        (root / "scripts").mkdir()
        (root / "bin").mkdir()
        for name in (runner, "with-test-home.sh", "test-command-denied.sh"):
            shutil.copy2(ROOT / "scripts" / name, root / "scripts" / name)
        for name in ("tmux", "script"):
            path = root / "bin" / name
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o700)
        path = root / "bin/cargo"
        path.write_text(f"#!{sys.executable}\n" + cargo_source)
        path.chmod(0o700)
        return root, {**os.environ, "PATH": str(root / "bin") + ":" + os.environ["PATH"]}

    def test_missing_scenario_fails_instead_of_zero_tests_passing(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "fake-test"
            executable.write_text("#!/bin/sh\nif [ \"$1\" = --list ]; then echo 'unrelated: test'; else exit 99; fi\n")
            executable.chmod(0o700)
            source = "import json\nprint(json.dumps(" + repr({"reason": "compiler-artifact", "profile": {"test": True}, "executable": str(executable)}) + "))\n"
            root, env = self.fixture(directory, "check-recovery.sh", source)
            result = subprocess.run([str(root / "scripts/check-recovery.sh")], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("missing exact recovery test", result.stderr)

    def test_failed_instrumented_tests_cannot_publish_success(self):
        with tempfile.TemporaryDirectory() as directory:
            source = """import sys
if sys.argv[1:] == ['llvm-cov', '--version']:
    print('cargo-llvm-cov 0.6.18')
    sys.exit(0)
sys.exit(23)
"""
            root, env = self.fixture(directory, "check-coverage.sh", source)
            report = root / "target/recovery-coverage"
            (report / "html").mkdir(parents=True)
            for path in (report / "summary.md", report / "coverage.json", report / "html/index.html"):
                path.write_text("stale successful report")
            result = subprocess.run([str(root / "scripts/check-coverage.sh")], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertFalse((report / "summary.md").exists())
            self.assertFalse((report / "coverage.json").exists())
            self.assertFalse((report / "html/index.html").exists())


if __name__ == "__main__":
    unittest.main()
