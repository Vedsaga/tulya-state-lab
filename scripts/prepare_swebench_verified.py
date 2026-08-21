#!/usr/bin/env python3
"""Prepare real base/child repository snapshots from SWE-bench Verified.

Uses only the Python standard library plus the local `git` executable.
The output manifest can be consumed by:

    cargo run --release -- --corpus-manifest <out>/manifest.tsv

Each child is the base repository with the dataset's gold source patch applied.
Snapshots contain deterministic encodings of tracked Git entries only; `.git`,
generated build products, and environment files are intentionally excluded.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import urllib.parse
import urllib.request

DATASET = "SWE-bench/SWE-bench_Verified"
CONFIG = "default"
SPLIT = "test"
ROWS_URL = "https://datasets-server.huggingface.co/rows"
PACK_MAGIC = b"TULYA_REPO_PACK_V1\0"


def run(cmd: list[str], *, cwd: Path | None = None, capture: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(
        cmd,
        cwd=cwd,
        check=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )


def fetch_rows(offset: int, length: int) -> list[dict]:
    params = urllib.parse.urlencode(
        {
            "dataset": DATASET,
            "config": CONFIG,
            "split": SPLIT,
            "offset": offset,
            "length": length,
        }
    )
    request = urllib.request.Request(
        f"{ROWS_URL}?{params}",
        headers={"User-Agent": "tulya-state-lab/real-corpus"},
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        payload = json.load(response)
    return [item["row"] for item in payload["rows"]]


def load_instances(offset: int, limit: int) -> list[dict]:
    rows: list[dict] = []
    cursor = offset
    while len(rows) < limit:
        take = min(100, limit - len(rows))
        batch = fetch_rows(cursor, take)
        if not batch:
            break
        rows.extend(batch)
        cursor += len(batch)
        if len(batch) < take:
            break
    return rows


def safe_name(value: str) -> str:
    return "".join(c if c.isalnum() or c in "._-" else "_" for c in value)


def ensure_repo(cache_root: Path, repo: str) -> Path:
    target = cache_root / "repos" / safe_name(repo)
    if target.exists():
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    print(f"cloning {repo} ...", flush=True)
    run(
        [
            "git",
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            "--quiet",
            f"https://github.com/{repo}.git",
            str(target),
        ]
    )
    return target


def ensure_commit(repo_dir: Path, commit: str) -> None:
    probe = subprocess.run(
        ["git", "-C", str(repo_dir), "cat-file", "-e", f"{commit}^{{commit}}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if probe.returncode == 0:
        return
    run(["git", "-C", str(repo_dir), "fetch", "--quiet", "origin", commit])


def tracked_entries(worktree: Path) -> list[tuple[str, str, str]]:
    result = run(
        ["git", "-C", str(worktree), "ls-files", "--stage", "-z"],
        capture=True,
    )
    entries: list[tuple[str, str, str]] = []
    for raw in result.stdout.split(b"\0"):
        if not raw:
            continue
        meta, path_bytes = raw.split(b"\t", 1)
        mode_b, sha_b, stage_b = meta.split(b" ", 2)
        if stage_b != b"0":
            raise RuntimeError(f"unexpected non-zero index stage for {path_bytes!r}")
        path = os.fsdecode(path_bytes)
        entries.append((path, mode_b.decode("ascii"), sha_b.decode("ascii")))
    entries.sort(key=lambda item: os.fsencode(item[0]))
    return entries


def pack_worktree(worktree: Path, output: Path) -> None:
    entries = tracked_entries(worktree)
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("wb") as out:
        out.write(PACK_MAGIC)
        out.write(struct.pack("<Q", len(entries)))
        for rel, mode, sha in entries:
            path_bytes = os.fsencode(rel)
            path = worktree / rel
            if mode == "160000":
                kind = 3
                content = sha.encode("ascii")
            elif mode == "120000":
                kind = 2
                content = os.fsencode(os.readlink(path))
            else:
                kind = 1
                content = path.read_bytes()
            out.write(struct.pack("<BIIQ", kind, int(mode, 8), len(path_bytes), len(content)))
            out.write(path_bytes)
            out.write(content)


def prepare_case(instance: dict, out_root: Path, cache_root: Path) -> tuple[str, str, str, str, str]:
    instance_id = instance["instance_id"]
    repo = instance["repo"]
    base_commit = instance["base_commit"]
    patch = instance["patch"]
    if not patch:
        raise RuntimeError(f"{instance_id}: dataset patch is empty")

    repo_dir = ensure_repo(cache_root, repo)
    ensure_commit(repo_dir, base_commit)
    worktree = cache_root / "worktrees" / safe_name(instance_id)
    if worktree.exists():
        shutil.rmtree(worktree)
    worktree.parent.mkdir(parents=True, exist_ok=True)

    case_dir = out_root / "cases"
    base_out = case_dir / f"{safe_name(instance_id)}.base.bin"
    child_out = case_dir / f"{safe_name(instance_id)}.child.bin"
    patch_file = cache_root / "patches" / f"{safe_name(instance_id)}.patch"
    patch_file.parent.mkdir(parents=True, exist_ok=True)
    patch_file.write_text(patch, encoding="utf-8")

    try:
        run(
            [
                "git",
                "-C",
                str(repo_dir),
                "worktree",
                "add",
                "--detach",
                "--quiet",
                str(worktree),
                base_commit,
            ]
        )
        pack_worktree(worktree, base_out)
        run(
            [
                "git",
                "-C",
                str(worktree),
                "apply",
                "--index",
                "--whitespace=nowarn",
                str(patch_file),
            ]
        )
        pack_worktree(worktree, child_out)
    finally:
        subprocess.run(
            ["git", "-C", str(repo_dir), "worktree", "remove", "--force", str(worktree)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if worktree.exists():
            shutil.rmtree(worktree, ignore_errors=True)

    return (
        instance_id,
        str(base_out.relative_to(out_root)),
        str(child_out.relative_to(out_root)),
        repo,
        base_commit,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=Path("traces/swebench-verified"))
    parser.add_argument("--cache", type=Path, default=Path(".trace-cache/swebench-verified"))
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--limit", type=int, default=20)
    args = parser.parse_args()

    if args.limit <= 0:
        parser.error("--limit must be positive")
    if args.offset < 0:
        parser.error("--offset must be non-negative")

    args.out = args.out.resolve()
    args.cache = args.cache.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    instances = load_instances(args.offset, args.limit)
    if len(instances) != args.limit:
        raise RuntimeError(f"requested {args.limit} rows but received {len(instances)}")

    manifest_rows: list[tuple[str, str, str, str, str]] = []
    for index, instance in enumerate(instances, 1):
        print(
            f"[{index}/{len(instances)}] {instance['instance_id']} ({instance['repo']})",
            flush=True,
        )
        manifest_rows.append(prepare_case(instance, args.out, args.cache))

    manifest = args.out / "manifest.tsv"
    with manifest.open("w", encoding="utf-8", newline="\n") as out:
        out.write("# case_id\tbase_snapshot\tchild_snapshot\trepo\tbase_commit\n")
        for row in manifest_rows:
            out.write("\t".join(row) + "\n")

    print(f"wrote {len(manifest_rows)} cases to {manifest}")
    print(
        "run: cargo run --release -- --corpus-manifest "
        f"{manifest} --verify-samples {min(16, len(manifest_rows))}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as exc:
        print(f"command failed ({exc.returncode}): {' '.join(exc.cmd)}", file=sys.stderr)
        raise
