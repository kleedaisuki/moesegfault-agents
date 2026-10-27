from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tomllib
from typing import Any
import zipfile

TARGETS: dict[str, str] = {"x86_64-unknown-linux-gnu": "linux", "x86_64-pc-windows-msvc": "win32"}
SHA: re.Pattern[str] = re.compile(r"[a-f0-9]{40}")
MAX_ARCHIVE_BYTES: int = 150 * 1024 * 1024


def command(args: list[str]) -> str:
    return subprocess.check_output(args, text=True).strip()


def api(path: str) -> dict[str, Any]:
    value: Any = json.loads(command(["gh", "api", path]))
    if not isinstance(value, dict):
        raise ValueError("Expected a GitHub API object")
    return value


def package_version() -> str:
    with Path("Cargo.toml").open("rb") as source:
        version: str = tomllib.load(source)["workspace"]["package"]["version"]
    if version != "0.3.0":
        raise ValueError("Native promotion is defined only for 0.3.0")
    return version


def names(version: str, target: str) -> tuple[str, str]:
    extension: str = "zip" if target.endswith("windows-msvc") else "tar.gz"
    archive: str = f"magents-{version}-{target}.{extension}"
    return archive, archive + ".sha256"


def file_hash(path: Path) -> str:
    digest: Any = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_info(archive: Path, target: str) -> dict[str, Any]:
    if archive.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("Native archive exceeds its bound")
    executable: str = "magents.exe" if target.endswith("windows-msvc") else "magents"
    expected: set[str] = {executable, "LICENSE", "BUILD-INFO.json"}
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as package:
            entries: list[zipfile.ZipInfo] = package.infolist()
            if len(entries) != 3 or {entry.filename for entry in entries} != expected:
                raise ValueError("Unexpected native ZIP members")
            if any(entry.is_dir() or entry.file_size > MAX_ARCHIVE_BYTES or (entry.external_attr >> 16) & 0o170000 == 0o120000 for entry in entries):
                raise ValueError("Invalid native ZIP member")
            if package.getinfo("BUILD-INFO.json").file_size > 16 * 1024:
                raise ValueError("Native metadata exceeds its bound")
            raw: bytes = package.read("BUILD-INFO.json")
    else:
        with tarfile.open(archive, "r:gz") as package:
            members: list[tarfile.TarInfo] = package.getmembers()
            if len(members) != 3 or {entry.name for entry in members} != expected or any(not entry.isfile() or entry.size > MAX_ARCHIVE_BYTES for entry in members):
                raise ValueError("Unexpected native tar members")
            member: tarfile.TarInfo = package.getmember("BUILD-INFO.json")
            if member.size > 16 * 1024:
                raise ValueError("Native metadata exceeds its bound")
            stream: Any = package.extractfile(member)
            if stream is None:
                raise ValueError("Native metadata is absent")
            raw = stream.read()
    info: Any = json.loads(raw)
    if not isinstance(info, dict):
        raise ValueError("Invalid native build metadata")
    return info


def validate_archive(directory: Path, version: str, target: str) -> dict[str, Any]:
    archive, checksum = names(version, target)
    digest: str = file_hash(directory / archive)
    if (directory / checksum).read_bytes() != f"{digest}  {archive}\n".encode("ascii"):
        raise ValueError("Native archive checksum or sidecar bytes differ")
    info: dict[str, Any] = build_info(directory / archive, target)
    if info.get("package") != "magents" or info.get("version") != version or info.get("target") != target or not SHA.fullmatch(str(info.get("commit", ""))):
        raise ValueError("Native archive source/version/target mismatch")
    with Path("rust-toolchain.toml").open("rb") as toolchain:
        channel: str = tomllib.load(toolchain)["toolchain"]["channel"]
    if not str(info.get("rustc", "")).startswith(f"rustc {channel} "):
        raise ValueError("Native compiler does not match the pinned toolchain")
    return info


def validate_receipt(plan: dict[str, Any], result: dict[str, Any], expected: dict[str, Any]) -> None:
    if plan.get("schema_version") != 1 or result.get("schema_version") != 1:
        raise ValueError("Unsupported native receipt schema")
    for field in ["source_sha", "version", "target", "platform", "profile", "validation_scope"]:
        if plan.get(field) != expected[field]:
            raise ValueError(f"Native receipt {field} differs")
    if result.get("source_sha") != expected["source_sha"] or result.get("job") != "rust":
        raise ValueError("Native result source or job differs")
    checks: list[dict[str, Any]] = expected["checks"]["rust"]
    selected: list[dict[str, Any]] = plan.get("checks", {}).get("rust", [])
    results: list[dict[str, Any]] = result.get("results", [])
    if len(checks) != 5 or len(selected) != 5 or len(results) != 5:
        raise ValueError("Both platform receipts must contain the five native release cases")
    for check, planned, actual in zip(checks, selected, results, strict=True):
        for field in ["id", "argv", "env"]:
            if check[field] != planned.get(field) or check[field] != actual.get(field):
                raise ValueError("Native release recipe differs")
        if actual.get("status") != "passed_current" or type(actual.get("executed_tests")) is not int or actual.get("executed_tests") != 1:
            raise ValueError("Native release case did not positively execute")


def source_matches(source: str, release: str) -> None:
    for sha in [source, release]:
        if not SHA.fullmatch(sha):
            raise ValueError("Invalid immutable native source")
        subprocess.run(["git", "cat-file", "-e", f"{sha}^{{commit}}"], check=True, stdout=subprocess.DEVNULL)
    script: str = "import {fingerprint} from './scripts/incremental-checks.mjs'; console.log(JSON.stringify(process.argv.slice(1).map(s=>fingerprint(s,'native'))));"
    hashes: list[str] = json.loads(command(["node", "--input-type=module", "-e", script, source, release]))
    if hashes[0] != hashes[1]:
        raise ValueError("Prebuilt native dependency fingerprint differs from release")


def expected_plan(version: str, target: str, source: str) -> dict[str, Any]:
    script: str = "import {readFileSync} from 'node:fs'; import {releaseChecks} from './scripts/magents-release-checks.mjs'; console.log(JSON.stringify(releaseChecks(process.argv[1],process.argv[2],process.argv[3],readFileSync('crates/magents/tests/space_responses.rs','utf8'),process.argv[4])));"
    return json.loads(command(["node", "--input-type=module", "-e", script, version, target, source, TARGETS[target]]))


def checked_run(repo: str, run_id: str) -> dict[str, Any]:
    if not re.fullmatch(r"[1-9][0-9]{0,14}", run_id):
        raise ValueError("Invalid native package run")
    run: dict[str, Any] = api(f"repos/{repo}/actions/runs/{run_id}")
    if run.get("status") != "completed" or run.get("conclusion") != "success" or run.get("path") != ".github/workflows/release-magents.yml" or run.get("head_repository", {}).get("full_name") != repo:
        raise ValueError("Package run is not a successful same-repository native workflow")
    jobs: list[dict[str, Any]] = api(f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100")["jobs"]
    for target in TARGETS:
        matching: list[dict[str, Any]] = [job for job in jobs if job.get("name") == target]
        if len(matching) != 1 or matching[0].get("conclusion") != "success":
            raise ValueError("Both actual native platform jobs must succeed")
    return run


def download_bundle(repo: str, run_id: str, version: str, release_source: str, directory: Path) -> list[Path]:
    run: dict[str, Any] = checked_run(repo, run_id)
    source: str = run["head_sha"]
    source_matches(source, release_source)
    artifacts: list[dict[str, Any]] = api(f"repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100")["artifacts"]
    files: list[Path] = []
    for target in TARGETS:
        package_dir: Path = directory / target / "package"
        proof_dir: Path = directory / target / "proof"
        for name, path in [(f"magents-{version}-{target}", package_dir), (f"native-checks-{version}-{target}", proof_dir)]:
            found: list[dict[str, Any]] = [item for item in artifacts if item.get("name") == name]
            if len(found) != 1 or found[0].get("expired") or found[0].get("size_in_bytes", 0) > MAX_ARCHIVE_BYTES:
                raise ValueError("Missing, expired or oversized native artifact")
            path.mkdir(parents=True, exist_ok=True)
            subprocess.run(["gh", "run", "download", run_id, "--repo", repo, "--name", name, "--dir", str(path)], check=True)
        info: dict[str, Any] = validate_archive(package_dir, version, target)
        if info["commit"] != source or info.get("workflow_run") != f"https://github.com/{repo}/actions/runs/{run_id}":
            raise ValueError("Native binary metadata does not identify its actual producer")
        plan: dict[str, Any] = json.loads((proof_dir / "plan.json").read_text(encoding="utf-8"))
        result: dict[str, Any] = json.loads((proof_dir / "rust.json").read_text(encoding="utf-8"))
        validate_receipt(plan, result, expected_plan(version, target, source))
        files.extend(package_dir / name for name in names(version, target))
    return files


def release_record(repo: str, tag: str) -> dict[str, Any]:
    value: Any = json.loads(command(["gh", "release", "view", tag, "--repo", repo, "--json", "tagName,assets"]))
    if value.get("tagName") != tag:
        raise ValueError("Existing release tag differs")
    return value


def select_mode(tag: str, reuse: str, asset_names: set[str], version: str) -> str:
    if not tag:
        if reuse:
            raise ValueError("Promotion requires an existing release tag")
        return "build"
    if tag != f"v{version}":
        raise ValueError("Release tag does not match actual package version")
    expected: set[str] = {name for target in TARGETS for name in names(version, target)}
    present: set[str] = asset_names & expected
    if present == expected:
        return "existing"
    if reuse:
        if not re.fullmatch(r"[1-9][0-9]{0,14}", reuse):
            raise ValueError("Invalid native package run")
        return "reuse"
    if present:
        raise ValueError("Partial native assets require their explicit original package run")
    return "build"


def main() -> None:
    mode: str = sys.argv[1]
    version: str = package_version()
    tag: str = os.environ.get("RELEASE_TAG", "")
    reuse: str = os.environ.get("REUSE_RUN", "")
    repo: str = os.environ.get("GITHUB_REPOSITORY", "")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("Invalid repository identity")
    record: dict[str, Any] = release_record(repo, tag) if tag else {}
    assets: set[str] = {item["name"] for item in record.get("assets", [])}
    selected: str = select_mode(tag, reuse, assets, version)
    if mode == "plan":
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            output.write(f"mode={selected}\n")
        print(json.dumps({"mode": selected, "tag": tag, "version": version}))
        return
    if mode != "promote" or selected == "build":
        raise ValueError("Native promotion requires validated reusable artifacts")
    directory: Path = Path(".temp/native-release-promotion")
    published: Path = directory / "existing"
    published.mkdir(parents=True, exist_ok=True)
    if selected == "existing":
        for target in TARGETS:
            for name in names(version, target):
                subprocess.run(["gh", "release", "download", tag, "--repo", repo, "--pattern", name, "--dir", str(published)], check=True)
        infos: list[dict[str, Any]] = [validate_archive(published, version, target) for target in TARGETS]
        run_ids: set[str] = set()
        for info in infos:
            match: re.Match[str] | None = re.fullmatch(rf"https://github\.com/{re.escape(repo)}/actions/runs/([1-9][0-9]{{0,14}})", str(info.get("workflow_run", "")))
            if match is None:
                raise ValueError("Native asset has no bounded same-repository producer")
            run_ids.add(match[1])
        if len(run_ids) != 1:
            raise ValueError("Native assets were not produced together")
        reuse = run_ids.pop()
    release_source: str = command(["git", "rev-parse", "HEAD"])
    if command(["git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"] ) != release_source:
        raise ValueError("Promotion checkout must be the immutable release tag")
    files: list[Path] = download_bundle(repo, reuse, version, release_source, directory / "producer")
    missing: list[Path] = []
    for file in files:
        if file.name not in assets:
            missing.append(file)
            continue
        existing: Path = published / file.name
        if not existing.exists():
            subprocess.run(["gh", "release", "download", tag, "--repo", repo, "--pattern", file.name, "--dir", str(published)], check=True)
        if file_hash(existing) != file_hash(file):
            raise ValueError("Existing release asset differs; refusing replacement")
    if selected == "reuse" and missing:
        subprocess.run(["gh", "release", "upload", tag, "--repo", repo, *map(str, missing)], check=True)
    receipt: dict[str, Any] = {"mode": selected, "source_run": reuse, "release_source": release_source,
        "assets": {file.name: file_hash(file) for file in files},
        "uploaded": len(missing) if selected == "reuse" else 0}
    (directory / "result.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt))


if __name__ == "__main__":
    main()
