# LangGraph / PostgresSaver product gate

This benchmark answers a different question from the in-process representation lab:

> Is a conventional LangGraph + PostgreSQL checkpoint stack already close enough to the branch/history economics we need that a new storage product is hard to justify?

It is intentionally **not** another Tulya backend.

## Pinned baseline

The benchmark pins:

- `langgraph==1.2.10`
- `langgraph-checkpoint-postgres==3.1.1`
- `psycopg[binary]==3.3.4`
- PostgreSQL 16 in the provided disposable Compose file

`DeltaChannel` is beta in LangGraph 1.2.x. The benchmark must report semantic failures rather than treating storage/latency numbers from a wrong historical state as valid evidence.

Two modes are measured:

1. `plain` — ordinary reducer-backed state. PostgreSQL checkpoints retain the accumulated channel value.
2. `delta` — the same logical state/update stream using `DeltaChannel`, with the default snapshot cadence of 1000 updates unless overridden.

The state is a byte string. Each update is encoded as a 16-byte `(start, delete_len)` header plus inserted bytes. The reducer applies the same exact edit semantics used by the Rust lab.

The Python workload generator mirrors `src/workload.rs`, including the xorshift RNG, structured base, edit payloads, historical-parent selection, and planned range reads. Sampled full states are reconstructed independently in Python and compared byte-for-byte with LangGraph `get_state()` results.

## What is measured

For each mode, the script uses an isolated PostgreSQL schema and reports:

- retained PostgreSQL relation bytes immediately after the base checkpoint;
- final retained relation bytes after all branches/checkpoints;
- physical relation growth per branch (tables + indexes + TOAST, excluding WAL and filesystem/container overhead);
- `graph.update_state()` p50/p95/p99;
- `graph.get_state()` + 4 KiB slice p50/p95/p99;
- reopen `get_state()` p50/p95/p99 from a new checkpointer connection;
- checkpoint/blob/write row counts;
- saver capability overrides (`copy_thread`, `prune`, delta-history);
- sampled byte-exact semantic verification;
- deterministic read checksum.

`get_state()+slice` is **not** a direct range-read benchmark. LangGraph hydrates the channel value and the script then slices it. Compare this as product-level historical state access, not as an apples-to-apples replacement for the Rust rope's direct 4 KiB range traversal.

## Setup

From the repository root:

```bash
docker compose -f benchmarks/langgraph/compose.yml down -v
docker compose -f benchmarks/langgraph/compose.yml up -d

python3 -m venv .venv-langgraph
source .venv-langgraph/bin/activate
python -m pip install --upgrade pip
python -m pip install -r benchmarks/langgraph/requirements.txt
```

The default URI is:

```text
postgresql://postgres:postgres@localhost:55432/postgres
```

Override it with `--db-uri` or `TULYA_BENCH_DATABASE_URI`.

## First run: small smoke

Do not start with the 1000 x 2 MiB plain-checkpoint run. First prove that the public LangGraph fork path behaves correctly:

```bash
python benchmarks/langgraph/bench_postgres.py \
  --workload small-edit \
  --branches 128 \
  --base-kib 256 \
  --edit-bytes 96 \
  --verify-samples 12 \
  --json-out results/langgraph-small-128.json
```

Both modes must report `semantic: PASS`. A `DeltaChannel` semantic failure on historical parents is a product/capability result, not something to benchmark around silently.

## Main branch-heavy gate

Only after the smoke is healthy:

```bash
python benchmarks/langgraph/bench_postgres.py \
  --workload small-edit \
  --branches 1000 \
  --base-mib 2 \
  --edit-bytes 96 \
  --verify-samples 16 \
  --json-out results/langgraph-small-1000.json
```

This may consume substantial PostgreSQL space in `plain` mode because it intentionally measures full accumulated checkpoints. Keep enough local disk available.

Then test the workload `DeltaChannel` is designed to help:

```bash
python benchmarks/langgraph/bench_postgres.py \
  --workload append-heavy \
  --branches 1000 \
  --base-mib 2 \
  --edit-bytes 512 \
  --verify-samples 16 \
  --json-out results/langgraph-append-1000.json
```

`append-heavy` still contains historical forks. If `delta` fails semantics there, run a diagnostic linear chain to separate "efficient on a linear history" from "correct for branch-native history":

```bash
python benchmarks/langgraph/bench_postgres.py \
  --mode delta \
  --workload linear-append \
  --branches 1000 \
  --base-mib 2 \
  --edit-bytes 512 \
  --verify-samples 16 \
  --json-out results/langgraph-linear-delta-1000.json
```

Do not use the linear diagnostic as a substitute for the branch-heavy product requirement.

## Interpretation

The current custom baseline is `persistent-piece-cow-interned`.

A LangGraph/Postgres result is compelling against a new storage product if it is operationally close enough while providing the surrounding persistence ecosystem. Conversely, a large storage/latency gap matters only if LangGraph also preserves the required historical-parent semantics.

Known upstream caveats must remain visible during interpretation:

- `DeltaChannel` is beta.
- Delta state depends on ancestor writes/snapshots for reconstruction.
- open 2026 issues exist around historical branching and older-history reconstruction.
- OSS `PostgresSaver` does not currently provide safe `keep_latest` pruning for DeltaChannel histories.

## Where zstd, Brotli, Git, and binary deltas fit

Do **not** mix these into the product gate as if they expose the same API.

After the LangGraph gate, use them as a separate storage-floor layer:

| baseline | test? | role |
| --- | --- | --- |
| zstd whole snapshot | yes | fast general compression floor; no structural sharing/history API |
| Brotli whole snapshot | yes, secondary | stronger/slower whole-object compression floor |
| Git pack/delta | yes on repository corpus | important repository-specific adversary with mature object/delta packing |
| xdelta/VCDIFF-style binary delta | only if a gap remains | generic parent-to-child delta floor; reconstruction chains and branch management must be counted |
| grammar/recompression | no for now | unjustified unless conventional baselines expose a repeatable large deficiency |

For compressed/delta baselines, measure both total stored bytes and the cost to recover a random historical state. A storage number without reconstruction, branch, and prune costs is not comparable to the online checkpoint substrate.

## Cleanup

```bash
docker compose -f benchmarks/langgraph/compose.yml down -v
rm -rf .venv-langgraph
```
