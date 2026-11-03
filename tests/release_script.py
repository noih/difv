import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
FAKE = r'''
import json, os, pathlib, sys
tool, args = pathlib.Path(sys.argv[1]).name, sys.argv[2:]
root = pathlib.Path(os.environ["RELEASE_TEST_ROOT"])
failure = os.environ.get("RELEASE_TEST_FAILURE", "")
late = (root / "published").exists()
with (root / "calls").open("a") as log:
    log.write(json.dumps([tool, *args]) + "\n")
if tool == "git":
    tap = args[:1] == ["-C"]
    if tap:
        assert args[1] == str(root.parent / "homebrew-tap"), args
        args = args[2:]
    if args == ["rev-parse", "--show-toplevel"]:
        print(root.parent if tap and failure == "wrong-root" else root.parent / "homebrew-tap" if tap else root)
    elif args == ["branch", "--show-current"]:
        print("other" if tap and failure == "wrong-branch" else "main")
    elif args == ["status", "--porcelain"]:
        if tap and (failure == "dirty" or late and failure == "late-dirty"): print(" M unrelated")
    elif args == ["remote", "get-url", "origin"]: print(os.environ["RELEASE_TEST_ORIGIN"])
    elif args in (["rev-parse", "HEAD"], ["rev-parse", "origin/main"]):
        print("changed" if tap and late and failure in ("late-head", "late-both") else "taphead" if tap else "difvhead")
    elif args[:3] == ["rev-parse", "-q", "--verify"]: sys.exit(1)
    elif args[:1] not in (["add"], ["commit"], ["tag"], ["push"], ["fetch"], ["pull"]): raise AssertionError(args)
elif tool == "gh":
    if args[:1] == ["api"]:
        if "homebrew-tap" in args[1]:
            if failure == "remote-error": sys.exit(1)
            print("changed" if late and failure == "late-both" else "drift" if failure == "drift" or late and failure == "late-drift" else "taphead")
        else: print("drift" if failure == "difv-drift" else "difvhead")
    elif args[:2] == ["run", "list"]: print("4242")
    elif args[:2] not in (["auth", "status"], ["run", "watch"]): raise AssertionError(args)
elif tool == "cargo":
    if args == ["publish", "--locked"]: (root / "published").touch()
elif tool == "curl":
    destination = pathlib.Path(args[args.index("-o") + 1])
    if str(destination) != "/dev/null": destination.write_text("generated formula\n")
elif tool == "brew":
    if args[:1] == ["--repository"]: print(root / "managed-tap")
elif tool != "sleep": raise AssertionError(tool)
'''


class ReleaseScriptTests(unittest.TestCase):
    def run_release(self, failure="", origin="https://github.com/noih/homebrew-tap.git", brew=True):
        temporary = tempfile.TemporaryDirectory(prefix="difv-release-script-")
        owned = Path(temporary.name).resolve()
        def cleanup():
            temporary.cleanup()
            self.assertFalse(owned.exists())
        self.addCleanup(cleanup)
        root = owned / "difv"
        (root / "scripts").mkdir(parents=True)
        for name in ("Cargo.toml", "Cargo.lock", "scripts/release"):
            shutil.copyfile(ROOT / name, root / name)
        if failure != "missing":
            (owned / "homebrew-tap").mkdir()
            if failure != "missing-formula": (owned / "homebrew-tap/Formula").mkdir()
        managed = root / "managed-tap/Formula/difv.rb"
        managed.parent.mkdir(parents=True)
        managed.write_text("managed sentinel\n")
        (root / "bin").mkdir()
        (root / "fake.py").write_text(FAKE)
        for tool in ("git", "gh", "cargo", "curl", "sleep", *(["brew"] if brew else [])):
            wrapper = root / "bin" / tool
            wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " " + shlex.quote(str(root / "fake.py")) + ' "$0" "$@"\n')
            wrapper.chmod(0o755)
        env = dict(os.environ, PATH=str(root / "bin") + ":/usr/bin:/bin", RELEASE_TEST_ROOT=str(root), RELEASE_TEST_FAILURE=failure, RELEASE_TEST_ORIGIN=origin)
        original = (root / "Cargo.toml").read_text()
        result = subprocess.run(["/bin/bash", str(root / "scripts/release"), "9.3.4"], cwd=root / "scripts", env=env, text=True, capture_output=True, timeout=20)
        calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
        self.assertEqual(managed.read_text(), "managed sentinel\n")
        for call in calls:
            self.assertFalse(any(arg in ("fetch", "pull", "clone") for arg in call), call)
            if call[0] == "brew": self.assertEqual(call[1:2], ["style"], call)
        return result, calls, root, original

    def test_developer_checkout_and_explicit_ssh_push(self):
        for origin in ("https://github.com/noih/homebrew-tap", "https://github.com/noih/homebrew-tap.git", "git@github.com:noih/homebrew-tap", "git@github.com:noih/homebrew-tap.git"):
            with self.subTest(origin=origin):
                result, calls, root, _ = self.run_release(origin=origin)
                self.assertEqual(result.returncode, 0, result.stderr)
                tap = root.parent / "homebrew-tap"
                self.assertEqual((tap / "Formula/difv.rb").read_text(), "generated formula\n")
                self.assertIn(["git", "-C", str(tap), "add", "Formula/difv.rb"], calls)
                self.assertIn(["git", "-C", str(tap), "commit", "-q", "-m", "difv 9.3.4"], calls)
                self.assertIn(["brew", "style", str(tap / "Formula/difv.rb")], calls)
                push = next(line for line in result.stdout.splitlines() if line.startswith("  git -C "))
                self.assertEqual(shlex.split(push), ["git", "-C", str(tap), "push", "git@github.com:noih/homebrew-tap.git", "main"])
                self.assertEqual(sum(c[:2] == ["git", "push"] for c in calls), 1)

    def test_preflight_stops_before_bump_or_publication(self):
        cases = [(failure, "https://github.com/noih/homebrew-tap.git") for failure in ("missing", "missing-formula", "wrong-root", "wrong-branch", "dirty", "drift", "remote-error", "difv-drift")]
        cases += [("", origin) for origin in ("https://github.com/other/homebrew-tap.git", "https://github.com.evil/noih/homebrew-tap.git", "git@github.com:noih/homebrew-taps.git", "https://github.com/noih/homebrew-tap.git/extra")]
        for failure, origin in cases:
            with self.subTest(failure=failure, origin=origin):
                result, calls, root, original = self.run_release(failure, origin)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((root / "Cargo.toml").read_text(), original)
                self.assertFalse(any(c[0] in ("cargo", "curl") or c[:2] in (["git", "tag"], ["git", "commit"], ["git", "push"]) for c in calls))
                self.assertFalse((root.parent / "homebrew-tap/Formula/difv.rb").exists())

    def test_tap_change_during_release_stops_before_copy(self):
        for failure in ("late-head", "late-both", "late-dirty", "late-drift"):
            with self.subTest(failure=failure):
                result, calls, root, _ = self.run_release(failure)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(["cargo", "publish", "--locked"], calls)
                self.assertFalse((root.parent / "homebrew-tap/Formula/difv.rb").exists())
                self.assertFalse(any(c[0] == "git" and c[1:2] == ["-C"] and "commit" in c for c in calls))

    def test_brew_remains_optional(self):
        result, calls, _, _ = self.run_release(brew=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c[0] == "brew" for c in calls))


if __name__ == "__main__":
    unittest.main()
