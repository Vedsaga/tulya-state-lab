#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path

import zstandard as zstd

FIXED_GIT_ENV = {
    "GIT_AUTHOR_NAME": "tulya-state-lab",
    "GIT_AUTHOR_EMAIL": "state-lab@example.invalid",
    "GIT_COMMITTER_NAME": "tulya-state-lab",
    "GIT_COMMITTER_EMAIL": "state-lab@example.invalid",
    "GIT_AUTHOR_DATE": "2000-01-01T00:00:00Z",
    "GIT_COMMITTER_DATE": "2000-01-01T00:00:00Z",
}


@dataclass
class Case:
    case_id: str
    base_path: Path
    child_path: Path


@dataclass
class StorageResult:
    name: str
    base_bytes: int
    final_bytes: int
    child_growth_bytes: int
    child_growth_bytes_per_case: float


def read_manifest(path: Path) -> list[Case]:
    root = path.parent
    cases: list[Case] = []
    for line_no, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < 3:
            raise ValueError(f"manifest line {line_no} must have at least 3 tab-separated columns")
        base = Path(fields[1])
        child = Path(fields[2])
        cases.append(
            Case(
                fields[0],
                base if base.is_absolute() else root / base,
                child if child.is_absolute() else root / child,
            )
        )
    if not cases:
        raise ValueError("manifest contains no cases")
    return cases


def safe_ref_component(value: str) -> str:
    value = re.sub(r"[^A-Za-z0-9._-]+", "_", value)
    value = value.strip("./") or "case"
    return value[:120]


def run_git(repo: Path, args: list[str], *, input_bytes: bytes | None = None) -> str:
    env = os.environ.copy()
    env.update(FIXED_GIT_ENV)
    proc = subprocess.run(
        ["git", f"--git-dir={repo}", *args],
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        env=env,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed ({proc.returncode}): {proc.stderr.decode(errors='replace')}"
        )
    return proc.stdout.decode().strip()


def write_snapshot_commit(repo: Path, data: bytes, message: str, parent: str | None = None) -> str:
    blob = run_git(repo, ["hash-object", "-w", "--stdin"], input_bytes=data)
    tree_line = f"100644 blob {blob}\tstate.bin\n".encode()
    tree = run_git(repo, ["mktree"], input_bytes=tree_line)
    args = ["commit-tree", tree]
    if parent is not None:
        args += ["-p", parent]
    return run_git(repo, args, input_bytes=(message + "\n").encode())


def object_db_bytes(repo: Path) -> int:
    objects = repo / "objects"
    return sum(p.stat().st_size for p in objects.rglob("*") if p.is_file())


def git_gc(repo: Path) -> None:
    run_git(repo, ["reflog", "expire", "--expire=now", "--all"])
    run_git(repo, ["gc", "--aggressive", "--prune=now"])


def measure_git(cases: list[Case], workdir: Path) -> StorageResult:
    repo = workdir / "git-floor.git"
    if repo.exists():
        shutil.rmtree(repo)
    subprocess.run(["git", "init", "--bare", "--quiet", str(repo)], check=True)
    run_git(repo, ["config", "gc.auto", "0"])

    base_commits: dict[str, str] = {}
    for index, case in enumerate(cases):
        data = case.base_path.read_bytes()
        commit = write_snapshot_commit(repo, data, f"base {case.case_id}")
        ref = f"refs/heads/base/{index:04d}-{safe_ref_component(case.case_id)}"
        run_git(repo, ["update-ref", ref, commit])
        base_commits[case.case_id] = commit

    git_gc(repo)
    base_bytes = object_db_bytes(repo)

    for index, case in enumerate(cases):
        data = case.child_path.read_bytes()
        commit = write_snapshot_commit(repo, data, f"child {case.case_id}", base_commits[case.case_id])
        ref = f"refs/heads/child/{index:04d}-{safe_ref_component(case.case_id)}"
        run_git(repo, ["update-ref", ref, commit])

    git_gc(repo)
    final_bytes = object_db_bytes(repo)
    growth = max(0, final_bytes - base_bytes)
    return StorageResult(
        "git-pack-delta-aggressive",
        base_bytes,
        final_bytes,
        growth,
        growth / len(cases),
    )


def measure_zstd(cases: list[Case], level: int) -> StorageResult:
    compressor = zstd.ZstdCompressor(level=level)
    base_bytes = 0
    child_bytes = 0
    for case in cases:
        base_bytes += len(compressor.compress(case.base_path.read_bytes()))
        child_bytes += len(compressor.compress(case.child_path.read_bytes()))
    final_bytes = base_bytes + child_bytes
    return StorageResult(
        f"zstd-independent-level-{level}",
        base_bytes,
        final_bytes,
        child_bytes,
        child_bytes / len(cases),
    )


def raw_sizes(cases: list[Case]) -> StorageResult:
    base_bytes = sum(case.base_path.stat().st_size for case in cases)
    child_bytes = sum(case.child_path.stat().st_size for case in cases)
    return StorageResult(
        "raw-independent-snapshots",
        base_bytes,
        base_bytes + child_bytes,
        child_bytes,
        child_bytes / len(cases),
    )


def fmt_mib(value: int) -> str:
    return f"{value / (1024 * 1024):.3f}"


def print_result(result: StorageResult) -> None:
    print(f"baseline: {result.name}")
    print(f"  base_storage_mib: {fmt_mib(result.base_bytes)}")
    print(f"  final_storage_mib: {fmt_mib(result.final_bytes)}")
    print(f"  child_growth_mib: {fmt_mib(result.child_growth_bytes)}")
    print(f"  child_growth_bytes_per_case: {result.child_growth_bytes_per_case:.1f}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus-manifest", type=Path, required=True)
    parser.add_argument("--workdir", type=Path, default=Path("target/storage-floor"))
    parser.add_argument("--zstd-level", type=int, default=3)
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()

    if shutil.which("git") is None:
        parser.error("git executable not found")

    cases = read_manifest(args.corpus_manifest.resolve())
    args.workdir.mkdir(parents=True, exist_ok=True)
    print("tulya-state-lab storage floors")
    print(f"manifest: {args.corpus_manifest}")
    print(f"cases: {len(cases)}")
    print("note: these are storage-only adversaries; they do not provide Tulya/LangGraph branch semantics")

    results = [raw_sizes(cases)]
    print("\nmeasuring zstd independent-snapshot floor...")
    results.append(measure_zstd(cases, args.zstd_level))
    print("measuring Git pack/delta floor...")
    results.append(measure_git(cases, args.workdir.resolve()))

    for result in results:
        print()
        print_result(result)

    git_result = results[-1]
    zstd_result = results[-2]
    if zstd_result.child_growth_bytes:
        print(
            f"\nGit/zstd child-growth ratio: "
            f"{git_result.child_growth_bytes / zstd_result.child_growth_bytes:.3f}x"
        )

    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "manifest": str(args.corpus_manifest),
            "cases": len(cases),
            "zstd_level": args.zstd_level,
            "results": [asdict(result) for result in results],
        }
        args.json_out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {args.json_out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
