#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import time
from dataclasses import asdict, dataclass
from pathlib import Path

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
class Latency:
    p50_ms: float
    p95_ms: float
    p99_ms: float


@dataclass
class GitOperationalResult:
    cases: int
    base_packed_bytes: int
    after_children_unrepacked_bytes: int
    unrepacked_child_growth_bytes: int
    unrepacked_child_growth_bytes_per_case: float
    child_write: Latency
    standard_repack_ms: float
    standard_final_bytes: int
    standard_child_growth_bytes: int
    standard_child_growth_bytes_per_case: float
    standard_read_full_blob: Latency
    aggressive_repack_ms: float
    aggressive_final_bytes: int
    aggressive_child_growth_bytes: int
    aggressive_child_growth_bytes_per_case: float
    aggressive_read_full_blob: Latency
    semantic_pass: bool


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


def run_git_bytes(repo: Path, args: list[str], *, input_bytes: bytes | None = None) -> bytes:
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
    return proc.stdout


def run_git(repo: Path, args: list[str], *, input_bytes: bytes | None = None) -> str:
    return run_git_bytes(repo, args, input_bytes=input_bytes).decode().strip()


def write_snapshot_commit(repo: Path, data: bytes, message: str, parent: str | None = None) -> tuple[str, str]:
    blob = run_git(repo, ["hash-object", "-w", "--stdin"], input_bytes=data)
    tree_line = f"100644 blob {blob}\tstate.bin\n".encode()
    tree = run_git(repo, ["mktree"], input_bytes=tree_line)
    args = ["commit-tree", tree]
    if parent is not None:
        args += ["-p", parent]
    commit = run_git(repo, args, input_bytes=(message + "\n").encode())
    return commit, blob


def object_db_bytes(repo: Path) -> int:
    objects = repo / "objects"
    return sum(p.stat().st_size for p in objects.rglob("*") if p.is_file())


def aggressive_gc(repo: Path) -> None:
    run_git(repo, ["reflog", "expire", "--expire=now", "--all"])
    run_git(repo, ["gc", "--aggressive", "--prune=now"])


def standard_repack(repo: Path) -> None:
    run_git(repo, ["repack", "-ad"])
    run_git(repo, ["prune-packed"])


def percentile_ns(values: list[int], pct: int) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[(len(ordered) - 1) * pct // 100]


def summarize_ms(values: list[int]) -> Latency:
    return Latency(*(percentile_ns(values, pct) / 1_000_000.0 for pct in (50, 95, 99)))


def sample_indices(total: int, count: int) -> list[int]:
    count = max(2, min(count, total))
    return [
        total - 1 if i + 1 == count else i * (total - 1) // (count - 1)
        for i in range(count)
    ]


def measure_reads(repo: Path, cases: list[Case], child_blobs: list[str], samples: int) -> tuple[Latency, bool]:
    values: list[int] = []
    semantic_pass = True
    for index in sample_indices(len(cases), samples):
        started = time.perf_counter_ns()
        actual = run_git_bytes(repo, ["cat-file", "blob", child_blobs[index]])
        values.append(time.perf_counter_ns() - started)
        if actual != cases[index].child_path.read_bytes():
            semantic_pass = False
    return summarize_ms(values), semantic_pass


def fmt_mib(value: int) -> str:
    return f"{value / (1024 * 1024):.3f}"


def measure(cases: list[Case], workdir: Path, read_samples: int) -> GitOperationalResult:
    seed_repo = workdir / "git-operational-seed.git"
    standard_repo = workdir / "git-operational-standard.git"
    aggressive_repo = workdir / "git-operational-aggressive.git"
    for path in (seed_repo, standard_repo, aggressive_repo):
        if path.exists():
            shutil.rmtree(path)

    subprocess.run(["git", "init", "--bare", "--quiet", str(seed_repo)], check=True)
    run_git(seed_repo, ["config", "gc.auto", "0"])

    base_commits: dict[str, str] = {}
    for index, case in enumerate(cases):
        commit, _blob = write_snapshot_commit(seed_repo, case.base_path.read_bytes(), f"base {case.case_id}")
        ref = f"refs/heads/base/{index:04d}-{safe_ref_component(case.case_id)}"
        run_git(seed_repo, ["update-ref", ref, commit])
        base_commits[case.case_id] = commit

    aggressive_gc(seed_repo)
    base_bytes = object_db_bytes(seed_repo)

    child_blobs: list[str] = []
    child_write_ns: list[int] = []
    for index, case in enumerate(cases):
        started = time.perf_counter_ns()
        commit, blob = write_snapshot_commit(
            seed_repo,
            case.child_path.read_bytes(),
            f"child {case.case_id}",
            base_commits[case.case_id],
        )
        ref = f"refs/heads/child/{index:04d}-{safe_ref_component(case.case_id)}"
        run_git(seed_repo, ["update-ref", ref, commit])
        child_write_ns.append(time.perf_counter_ns() - started)
        child_blobs.append(blob)

    unrepacked_bytes = object_db_bytes(seed_repo)
    unrepacked_growth = max(0, unrepacked_bytes - base_bytes)

    shutil.copytree(seed_repo, standard_repo)
    shutil.copytree(seed_repo, aggressive_repo)

    started = time.perf_counter_ns()
    standard_repack(standard_repo)
    standard_repack_ns = time.perf_counter_ns() - started
    standard_bytes = object_db_bytes(standard_repo)
    standard_growth = max(0, standard_bytes - base_bytes)
    standard_read, standard_ok = measure_reads(
        standard_repo, cases, child_blobs, read_samples
    )

    started = time.perf_counter_ns()
    aggressive_gc(aggressive_repo)
    aggressive_repack_ns = time.perf_counter_ns() - started
    aggressive_bytes = object_db_bytes(aggressive_repo)
    aggressive_growth = max(0, aggressive_bytes - base_bytes)
    aggressive_read, aggressive_ok = measure_reads(
        aggressive_repo, cases, child_blobs, read_samples
    )

    return GitOperationalResult(
        cases=len(cases),
        base_packed_bytes=base_bytes,
        after_children_unrepacked_bytes=unrepacked_bytes,
        unrepacked_child_growth_bytes=unrepacked_growth,
        unrepacked_child_growth_bytes_per_case=unrepacked_growth / len(cases),
        child_write=summarize_ms(child_write_ns),
        standard_repack_ms=standard_repack_ns / 1_000_000.0,
        standard_final_bytes=standard_bytes,
        standard_child_growth_bytes=standard_growth,
        standard_child_growth_bytes_per_case=standard_growth / len(cases),
        standard_read_full_blob=standard_read,
        aggressive_repack_ms=aggressive_repack_ns / 1_000_000.0,
        aggressive_final_bytes=aggressive_bytes,
        aggressive_child_growth_bytes=aggressive_growth,
        aggressive_child_growth_bytes_per_case=aggressive_growth / len(cases),
        aggressive_read_full_blob=aggressive_read,
        semantic_pass=standard_ok and aggressive_ok,
    )


def print_latency(label: str, latency: Latency) -> None:
    print(
        f"  {label} p50/p95/p99 ms: "
        f"{latency.p50_ms:.3f} / {latency.p95_ms:.3f} / {latency.p99_ms:.3f}"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus-manifest", type=Path, required=True)
    parser.add_argument("--workdir", type=Path, default=Path("target/storage-floor-operational"))
    parser.add_argument("--read-samples", type=int, default=16)
    parser.add_argument("--json-out", type=Path)
    args = parser.parse_args()

    if shutil.which("git") is None:
        parser.error("git executable not found")
    if args.read_samples <= 0:
        parser.error("--read-samples must be positive")

    cases = read_manifest(args.corpus_manifest.resolve())
    args.workdir.mkdir(parents=True, exist_ok=True)

    print("tulya-state-lab Git operational decomposition")
    print(f"manifest: {args.corpus_manifest}")
    print(f"cases: {len(cases)}")
    print("note: full-blob Git materialization is not a range-read implementation")

    result = measure(cases, args.workdir.resolve(), args.read_samples)

    print(f"\nbase_aggressively_packed_mib: {fmt_mib(result.base_packed_bytes)}")
    print(f"after_children_before_repack_mib: {fmt_mib(result.after_children_unrepacked_bytes)}")
    print(f"unrepacked_child_growth_mib: {fmt_mib(result.unrepacked_child_growth_bytes)}")
    print(f"unrepacked_child_growth_bytes_per_case: {result.unrepacked_child_growth_bytes_per_case:.1f}")
    print_latency("child_write", result.child_write)

    print("\nstandard repack (-ad):")
    print(f"  elapsed_ms: {result.standard_repack_ms:.3f}")
    print(f"  final_storage_mib: {fmt_mib(result.standard_final_bytes)}")
    print(f"  child_growth_bytes_per_case: {result.standard_child_growth_bytes_per_case:.1f}")
    print_latency("cat_file_full_blob", result.standard_read_full_blob)

    print("\naggressive gc/repack:")
    print(f"  elapsed_ms: {result.aggressive_repack_ms:.3f}")
    print(f"  final_storage_mib: {fmt_mib(result.aggressive_final_bytes)}")
    print(f"  child_growth_bytes_per_case: {result.aggressive_child_growth_bytes_per_case:.1f}")
    print_latency("cat_file_full_blob", result.aggressive_read_full_blob)

    print(f"\nsemantic sampled blob reconstruction: {'PASS' if result.semantic_pass else 'FAIL'}")

    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "manifest": str(args.corpus_manifest),
            "result": asdict(result),
        }
        args.json_out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {args.json_out}")

    return 0 if result.semantic_pass else 2


if __name__ == "__main__":
    raise SystemExit(main())
