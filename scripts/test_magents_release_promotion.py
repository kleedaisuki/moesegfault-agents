from __future__ import annotations

import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import tarfile
from types import ModuleType
from typing import Any
import unittest
from unittest.mock import patch
import zipfile

SPEC: Any = importlib.util.spec_from_file_location("native_promotion", Path(__file__).with_name("magents-release-promotion.py"))
promotion: ModuleType = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(promotion)
SOURCE: str = "a" * 40
VERSION: str = "0.3.0"
REPO: str = "owner/repo"
RUN: str = "1234"


class NativePromotionTests(unittest.TestCase):
    def test_plan_reuses_or_validates_without_rebuilding_existing_assets(self) -> None:
        expected: set[str] = {name for target in promotion.TARGETS for name in promotion.names(VERSION, target)}
        self.assertEqual(promotion.select_mode("", "", set(), VERSION), "build")
        self.assertEqual(promotion.select_mode("v0.3.0", RUN, set(), VERSION), "reuse")
        self.assertEqual(promotion.select_mode("v0.3.0", RUN, {next(iter(expected))}, VERSION), "reuse")
        self.assertEqual(promotion.select_mode("v0.3.0", "", expected, VERSION), "existing")
        for tag, run, assets in [("", RUN, set()), ("v0.2.0", RUN, set()), ("v0.3.0", "", {next(iter(expected))}), ("v0.3.0", "not-a-run", set())]:
            with self.assertRaises(ValueError):
                promotion.select_mode(tag, run, assets, VERSION)

    def test_native_receipts_require_all_five_actual_platform_release_cases(self) -> None:
        for target in promotion.TARGETS:
            expected: dict[str, Any] = promotion.expected_plan(VERSION, target, SOURCE)
            plan: dict[str, Any] = copy.deepcopy(expected)
            result: dict[str, Any] = {"schema_version": 1, "source_sha": SOURCE, "job": "rust", "results": [dict(check, status="passed_current", executed_tests=1) for check in plan["checks"]["rust"]]}
            promotion.validate_receipt(plan, result, expected)
            for field, value in [("platform", "wrong"), ("profile", "debug"), ("source_sha", "b" * 40)]:
                wrong: dict[str, Any] = copy.deepcopy(plan)
                wrong[field] = value
                with self.assertRaises(ValueError):
                    promotion.validate_receipt(wrong, result, expected)
            for field, value in [("executed_tests", 0), ("status", "failed_current"), ("argv", ["cargo", "test"])]:
                wrong_result: dict[str, Any] = copy.deepcopy(result)
                wrong_result["results"][0][field] = value
                with self.assertRaises(ValueError):
                    promotion.validate_receipt(plan, wrong_result, expected)
            with self.assertRaises(ValueError):
                promotion.validate_receipt(plan, dict(result, results=result["results"][:-1]), expected)

    def make_archive(self, directory: Path, target: str, *, extra: bool = False) -> Path:
        name: str = promotion.names(VERSION, target)[0]
        archive: Path = directory / name
        info: bytes = json.dumps({"package": "magents", "version": VERSION, "target": target,
            "commit": SOURCE, "source_ref": "refs/heads/frozen-prebuild", "rustc": "rustc 1.98.1 (fixture)",
            "workflow_run": f"https://github.com/{REPO}/actions/runs/{RUN}"}).encode()
        members: dict[str, bytes] = {"magents.exe" if target.endswith("windows-msvc") else "magents": b"fixture-not-executed", "LICENSE": b"license", "BUILD-INFO.json": info}
        if extra:
            members["../escape"] = b"no"
        if name.endswith(".zip"):
            with zipfile.ZipFile(archive, "w") as package:
                for path, content in members.items():
                    package.writestr(path, content)
        else:
            with tarfile.open(archive, "w:gz") as package:
                for path, content in members.items():
                    entry: tarfile.TarInfo = tarfile.TarInfo(path)
                    entry.size = len(content)
                    package.addfile(entry, io.BytesIO(content))
        archive.with_name(name + ".sha256").write_bytes(f"{promotion.file_hash(archive)}  {name}\n".encode("ascii"))
        return archive

    def test_archives_preserve_actual_source_and_reject_checksum_or_member_changes(self) -> None:
        for target in promotion.TARGETS:
            with tempfile.TemporaryDirectory(dir=".temp") as temporary:
                directory: Path = Path(temporary)
                archive: Path = self.make_archive(directory, target)
                original: bytes = archive.read_bytes()
                info: dict[str, Any] = promotion.validate_archive(directory, VERSION, target)
                self.assertEqual(info["source_ref"], "refs/heads/frozen-prebuild")
                self.assertEqual(info["commit"], SOURCE)
                self.assertEqual(archive.read_bytes(), original)
                checksum: Path = archive.with_name(archive.name + ".sha256")
                checksum.write_bytes(checksum.read_bytes().replace(b"\n", b"\r\n"))
                with self.assertRaises(ValueError):
                    promotion.validate_archive(directory, VERSION, target)
                self.make_archive(directory, target, extra=True)
                with self.assertRaises(ValueError):
                    promotion.validate_archive(directory, VERSION, target)

    def test_only_successful_same_repository_matrix_producers_are_eligible(self) -> None:
        run: dict[str, Any] = {"status": "completed", "conclusion": "success", "path": ".github/workflows/release-magents.yml", "head_repository": {"full_name": REPO}, "head_sha": SOURCE}
        jobs: dict[str, Any] = {"jobs": [{"name": target, "conclusion": "success"} for target in promotion.TARGETS]}
        with patch.object(promotion, "api", side_effect=[run, jobs]):
            self.assertEqual(promotion.checked_run(REPO, RUN)["head_sha"], SOURCE)
        for wrong in [dict(run, conclusion="failure"), dict(run, path="other.yml"), dict(run, head_repository={"full_name": "fork/repo"})]:
            with patch.object(promotion, "api", return_value=wrong), self.assertRaises(ValueError):
                promotion.checked_run(REPO, RUN)
        jobs["jobs"][0]["conclusion"] = "failure"
        with patch.object(promotion, "api", side_effect=[run, jobs]), self.assertRaises(ValueError):
            promotion.checked_run(REPO, RUN)

    def test_native_fingerprint_not_documentation_sha_controls_compatible_promotion(self) -> None:
        with patch.object(promotion.subprocess, "run"), patch.object(promotion, "command", return_value='["same","same"]'):
            promotion.source_matches(SOURCE, "b" * 40)
        with patch.object(promotion.subprocess, "run"), patch.object(promotion, "command", return_value='["native-old","native-changed"]'), self.assertRaises(ValueError):
            promotion.source_matches(SOURCE, "b" * 40)

    def test_published_event_has_no_clobber_or_rebuild_path_after_complete_promotion(self) -> None:
        workflow: str = Path(".github/workflows/release-magents.yml").read_text(encoding="utf-8")
        script: str = Path("scripts/magents-release-promotion.py").read_text(encoding="utf-8")
        self.assertIn("if: needs.prepare.outputs.mode == 'build'", workflow)
        self.assertIn("needs.prepare.outputs.mode == 'reuse' || needs.prepare.outputs.mode == 'existing'", workflow)
        self.assertNotIn("--clobber", script)
        self.assertNotIn('"release", "create"', script)
        self.assertNotIn('"release", "edit"', script)
        self.assertIn('if selected == "reuse" and missing:', script)
        self.assertIn("fetch-depth: 0", workflow)


if __name__ == "__main__":
    Path(".temp").mkdir(exist_ok=True)
    unittest.main()
