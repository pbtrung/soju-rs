"""Hermetic release helper tests: every git/gh invocation uses a local stub.

Run with python3 tests/release_scripts.py. No remote or local Git repository is
modified; each scenario runs against copied manifests/workflows in a temp dir.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parent.parent
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
TAG = "v" + VERSION
NEXT_VERSION = ".".join([*VERSION.split(".")[:2], str(int(VERSION.split(".")[2]) + 1)])
STUB = r'''
import json
import os
from pathlib import Path
import sys

tool, args = Path(sys.argv[0]).name, sys.argv[1:]
case = json.loads(os.environ["RELEASE_CASE"])
with open(os.environ["RELEASE_LOG"], "a") as log:
    log.write(json.dumps([tool, *args]) + "\n")

if tool == "git":
    if args == ["branch", "--show-current"]:
        print(case.get("branch", "main"))
    elif args == ["status", "--porcelain"]:
        if case.get("dirty"):
            print(" M README.md")
    elif args == ["remote", "get-url", "origin"]:
        print("https://example.invalid/owner/soju-rs.git")
    elif args[:1] == ["fetch"]:
        pass
    elif args[:1] == ["merge-base"]:
        sys.exit(1 if case.get("behind") else 0)
    elif args[:1] == ["ls-remote"]:
        if case.get("tag_error"):
            sys.exit(128)
        if case.get("tag_exists"):
            print("old-object\t" + args[-1])
        else:
            sys.exit(2)
    elif args[:1] in (["add"], ["commit"], ["update-ref"]):
        pass
    elif args[:1] == ["push"]:
        sys.exit(1 if case.get("push_error") else 0)
    elif args == ["rev-parse", "HEAD"]:
        print("new-object")
    else:
        raise SystemExit("Unexpected git call: " + repr(args))
elif tool == "gh":
    if args == ["auth", "status"]:
        pass
    elif args[:2] == ["repo", "view"]:
        print("owner/soju-rs")
    elif args[:1] == ["api"] and "actions/workflows/" in args[1]:
        if case.get("workflow_error"):
            sys.exit(1)
        print(case.get("workflow_state", "active"))
    elif args[:2] == ["workflow", "view"]:
        if case.get("view_error"):
            sys.exit(1)
        definition = Path(".github/workflows/release.yml").read_text()
        if case.get("workflow_changed"):
            definition = definition.replace("publish:", "other_input:")
        print(definition, end="")
    elif args[:1] == ["api"] and "/releases?" in args[2]:
        if case.get("release_api_error"):
            sys.exit(1)
        if case.get("release_exists"):
            print("123")
    elif args[:2] == ["release", "delete"]:
        pass
    elif args[:2] == ["workflow", "run"]:
        if args[2] != "release.yml" or "publish=true" not in args:
            raise SystemExit("Incorrect dispatch: " + repr(args))
    else:
        raise SystemExit("Unexpected gh call: " + repr(args))
'''


class ReleaseScripts(unittest.TestCase):
    def run_helper(self, helper, **case):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            (work / "scripts").mkdir()
            (work / ".github/workflows").mkdir(parents=True)
            (work / "bin").mkdir()
            for name in ["release-new.sh", "release-replace.sh", "release-common.sh"]:
                shutil.copyfile(ROOT / "scripts" / name, work / "scripts" / name)
            for name in ["Cargo.toml", "Cargo.lock", ".github/workflows/release.yml"]:
                shutil.copyfile(ROOT / name, work / name)
            if case.get("missing_local"):
                (work / ".github/workflows/release.yml").unlink()
            for tool in ["git", "gh"]:
                stub = work / "bin" / tool
                stub.write_text(f"#!{sys.executable}\n" + STUB)
                stub.chmod(0o755)
            env = dict(os.environ)
            env.update(
                PATH=str(work / "bin") + os.pathsep + env["PATH"],
                RELEASE_CASE=json.dumps(case),
                RELEASE_LOG=str(work / "calls.jsonl"),
            )
            args = ["bash", str(work / "scripts" / helper)]
            if helper == "release-new.sh":
                args.append(NEXT_VERSION)
            result = subprocess.run(args, env=env, capture_output=True, text=True, check=False)
            log = work / "calls.jsonl"
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            changed = (work / "Cargo.toml").read_bytes() != (ROOT / "Cargo.toml").read_bytes()
            return result, calls, changed

    def assert_no_mutations(self, calls):
        for call in calls:
            if call[0] == "git":
                self.assertNotIn(call[1], ["add", "commit", "push", "update-ref"], calls)
            else:
                self.assertNotEqual(call[1:3], ["release", "delete"], calls)
                self.assertNotEqual(call[1:3], ["workflow", "run"], calls)

    def test_failed_preflight_prevents_all_release_mutations(self):
        for helper in ["release-new.sh", "release-replace.sh"]:
            for case in [
                {"missing_local": True},
                {"workflow_error": True},
                {"workflow_state": "disabled_manually"},
                {"view_error": True},
                {"workflow_changed": True},
                {"dirty": True},
                {"branch": "feature"},
                {"behind": True},
                {"tag_error": True},
            ]:
                with self.subTest(helper=helper, case=case):
                    result, calls, changed = self.run_helper(helper, **case)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assert_no_mutations(calls)
                    self.assertFalse(changed)

    def test_new_release_dispatches_existing_workflow_after_push(self):
        result, calls, changed = self.run_helper("release-new.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(changed)
        self.assertLess(calls.index(["git", "push", "origin", "HEAD:refs/heads/main"]),
                        next(i for i, call in enumerate(calls) if call[:3] == ["gh", "workflow", "run"]))
        self.assertEqual(calls[-1], ["gh", "workflow", "run", "release.yml", "--ref", "main",
                                   "-F", "publish=true", "--repo", "owner/soju-rs"])

    def test_new_release_rejects_existing_tag(self):
        result, calls, changed = self.run_helper("release-new.sh", tag_exists=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_no_mutations(calls)
        self.assertFalse(changed)

    def test_replacement_preserves_lease_and_mutation_order(self):
        for exists in [False, True]:
            with self.subTest(exists=exists):
                result, calls, changed = self.run_helper("release-replace.sh", tag_exists=exists,
                                                        release_exists=exists)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(changed)
                lease = f"--force-with-lease=refs/tags/{TAG}:" + ("old-object" if exists else "")
                push = calls.index(["git", "push", "origin", "HEAD:refs/heads/main"])
                tag = calls.index(["git", "push", lease, "origin", f"HEAD:refs/tags/{TAG}"])
                dispatch = next(i for i, call in enumerate(calls) if call[:3] == ["gh", "workflow", "run"])
                self.assertLess(push, tag)
                self.assertLess(tag, dispatch)
                if exists:
                    delete = next(i for i, call in enumerate(calls) if call[:3] == ["gh", "release", "delete"])
                    self.assertLess(tag, delete)
                    self.assertLess(delete, dispatch)

    def test_replacement_api_failure_preserves_release(self):
        result, calls, _ = self.run_helper("release-replace.sh", release_api_error=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_no_mutations(calls)

    def test_rejected_branch_push_never_deletes_or_dispatches(self):
        for helper in ["release-new.sh", "release-replace.sh"]:
            with self.subTest(helper=helper):
                result, calls, _ = self.run_helper(helper, push_error=True, release_exists=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls[-1], ["git", "push", "origin", "HEAD:refs/heads/main"])


if __name__ == "__main__":
    unittest.main()
