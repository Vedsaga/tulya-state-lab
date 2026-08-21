# Storage-floor adversaries

These benchmarks answer a narrower question than the Rust representation or LangGraph gates:

> How small can the same snapshot corpus get under conventional compression/delta storage when branch-native edit/read semantics are not required?

They are **not** substitute backends. They do not receive credit for historical random access, arbitrary-parent edit semantics, pruning, or branch operations they do not implement.

## Current floors

### Git pack/delta

The benchmark creates a temporary bare Git object database and stores each packed repository snapshot as a blob under a one-file tree.

1. Every base snapshot is committed as an independent root and kept reachable.
2. Git runs `gc --aggressive --prune=now`.
3. Object-database bytes are measured as retained base storage.
4. Every child snapshot is committed with its corresponding base commit as parent; both base and child refs remain reachable.
5. Git aggressively repacks again.
6. Final object-database bytes and incremental child growth are measured.

This is intentionally a strong conventional delta-compression floor for repository-like snapshots. Git is free to delta similar blobs across the object database; the measurement includes blob, tree, commit, pack-index, and other object-database files.

### zstd independent snapshots

Each base and child snapshot is compressed independently with `zstandard`. No dictionary, cross-snapshot deduplication, or delta relation is used.

This is a whole-object compression floor, not a version-sharing scheme.

## Run

The current Python dependency is pinned in `requirements.txt`.

```bash
python -m pip install -r benchmarks/storage_floor/requirements.txt

python benchmarks/storage_floor/bench_corpus.py \
  --corpus-manifest traces/swebench-verified-diverse/manifest.tsv \
  --zstd-level 3 \
  --json-out results/storage-floor-swebench-diverse.json
```

The script reuses the already-prepared 24-case SWE-bench corpus; it does not clone repositories again.

## Interpretation

Keep three layers separate:

- **branch-native representation:** persistent COW / CDC lab results;
- **product/system baseline:** LangGraph + PostgresSaver;
- **storage floor:** Git pack/delta and independent zstd compression.

If COW is materially larger than Git on global cold storage but still wins branch growth and latency, that indicates a possible tiering/dedup opportunity rather than evidence that Git itself should be the online checkpoint engine.

Brotli and xdelta/VCDIFF remain optional follow-ups. Add them only if Git/zstd expose a material unexplained gap worth decomposing further.
