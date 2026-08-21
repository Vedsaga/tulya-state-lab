# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1 — frozen negative result for AVL byte rope

The first controlled workload compared three representations under the same deterministic arbitrary-parent branch/edit/read plan:

- **persistent AVL byte rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, 4 KiB-style byte leaves, direct range traversal. Splitting a leaf copies the affected leaf fragments.
- **persistent COW piece rope** — persistent balanced-tree indexing whose leaves are slices of immutable shared buffers. Splitting a piece allocates metadata only; unchanged bytes are never copied.
- **incremental windowed CDC** — fixed-window rolling content-defined chunking with exact-byte deduplication and flat per-version manifests. Edits re-chunk only a repair region and reuse the untouched old suffix after exact boundary resynchronization.

At 1,000 branches over a 2 MiB base state (seed `0x5eed1234d15ca11e`, max edit 96 bytes, 4 KiB reads), all three backends produced the same checksum and sampled historical versions matched byte-for-byte.

| backend | retained growth / branch | edit p95 | read p95 |
| --- | ---: | ---: | ---: |
| persistent AVL byte rope | 4,841.7 B | 9.168 us | 3.496 us |
| persistent COW piece rope | **892.6 B** | **3.186 us** | **3.296 us** |
| incremental windowed CDC | 20,686.8 B | 327.548 us | 1.102 us |

The AVL byte rope is **dominated** on this workload and must not be promoted as a Tulya architecture or novelty claim.

### Quarantined early CDC results

Results produced before commit `2f4147cdc95b6e3347b10983a9f846990b7d2684` used a weaker chunk-prefix fingerprint that reset at every chunk boundary. Those runs are diagnostics only and **must not be used as evidence that AVL beats a strong CDC baseline**.

## Phase 2 — frozen synthetic result

The workload families were:

| workload | purpose |
| --- | --- |
| `small-edit` | Phase-1 arbitrary-parent insert/delete/replace baseline |
| `append-heavy` | mostly extend latest state, sometimes fork historical parents |
| `cross-template` | unrelated branches inject identical large structured payloads |
| `large-rewrite` | arbitrary historical parents replace large regions with fresh content |

Plain COW won the small-edit and large-rewrite workloads and remained competitive on append-heavy history. The deliberately CDC-favorable `cross-template` workload was the only case where CDC initially stored less: about 52.9 KiB/branch versus 67.8 KiB/branch for plain COW, at roughly two orders of magnitude worse p95 edit latency.

That gap was then attacked with the simplest conventional mechanism: **exact immutable-buffer interning**. `persistent-piece-cow-interned` keeps the same persistent piece tree and only changes backing-buffer allocation:

1. hash the candidate immutable buffer;
2. inspect candidates with the same hash;
3. require exact byte-for-byte equality before reuse;
4. otherwise allocate a new `Arc<[u8]>`.

The hash is only a lookup accelerator; collisions cannot cause false sharing. The index stores weak references and does not keep dead payloads alive.

### Interned COW result

On `cross-template` (500 children, 2 MiB base, 64 KiB repeated payload):

| backend | retained growth / branch | edit p95 | read p95 |
| --- | ---: | ---: | ---: |
| persistent COW + exact interning | **2,816.7 B** | **94.210 us** | 5.862 us |
| incremental windowed CDC | 52,927.7 B | 11,053.201 us | **1.493 us** |

COW + exact interning therefore used about **18.8x less retained growth** while retaining roughly **117x lower p95 edit latency**. CDC retained faster range reads, but no longer had a storage case.

On `large-rewrite` (500 children, 8 MiB base, 64 KiB fresh rewrites):

| backend | retained growth / branch | edit p95 | read p95 |
| --- | ---: | ---: | ---: |
| persistent COW + exact interning | **67,986.3 B** | **123.786 us** | 1.673 us |
| incremental windowed CDC | 102,180.5 B | 734.932 us | **1.323 us** |

Hashing unique 64 KiB payloads costs CPU relative to plain COW, but even here interned COW remained smaller and materially faster to edit than CDC.

**Synthetic conclusion:** do not implement grammar/recompression next. The baseline to beat is conventional persistent COW plus exact content interning.

## Phase 3 — real snapshot corpus

The harness now accepts real independent `(base snapshot, child snapshot)` pairs through a tab-separated manifest:

```text
case_id<TAB>base_snapshot_path<TAB>child_snapshot_path
```

Extra columns are ignored, so source metadata can travel with the corpus.

All base snapshots are loaded first. The harness records retained base storage, then applies one exact child transition per case and reports **child growth separately from base storage**. Each transition is derived as the shortest single contiguous replacement obtained from the longest common prefix and suffix of the two byte snapshots.

That representation is byte-exact but conservative for patches touching multiple distant files: the replacement spans from the first changed byte to the last changed byte. Treat this as a snapshot-diff gate, not as a claim that it captures an optimal multi-edit script.

Run an already-prepared corpus with:

```bash
cargo run --release -- \
  --corpus-manifest traces/example/manifest.tsv \
  --verify-samples 16
```

The report includes total base-build time, child edit/read percentiles, base retained storage, final retained storage, child growth per case, representation allocation per case, checksum, and sampled byte-exact semantic verification across all three backends.

## First external gate: SWE-bench Verified

`scripts/prepare_swebench_verified.py` prepares real repository snapshots from SWE-bench Verified using only Python's standard library and Git. It fetches dataset rows, checks out each `base_commit`, applies the gold source patch, and packs tracked repository entries deterministically. `.git`, build products, and environment state are excluded.

Start with 20 instances rather than all 500:

```bash
python3 scripts/prepare_swebench_verified.py \
  --limit 20 \
  --out traces/swebench-verified

cargo test --all-targets

cargo run --release -- \
  --corpus-manifest traces/swebench-verified/manifest.tsv \
  --verify-samples 16
```

The preparer caches cloned repositories under `.trace-cache/swebench-verified` by default. Both `.trace-cache/` and `traces/` are ignored by Git.

Do not scale to all 500 instances until the first 20 cases compile, prepare, run, and make semantic sense.

## What is measured

Synthetic workloads report build time, edit and range-read p50/p95/p99, retained payload and explicit metadata, retained growth per branch, lifetime representation allocation, live/lifetime structural objects, a read checksum, and sampled cross-backend semantic equality.

Real corpora additionally separate retained base storage from child-version growth.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, process/runtime overhead, and temporary buffers are excluded. The lifetime-allocation metric measures allocations retained by or created for the representation itself; it is not a full heap-allocation or physical-write-amplification measurement.

## Decision rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

The baseline to beat is **persistent COW pieces with exact immutable-buffer interning**. If a more elaborate candidate comes within roughly 20–25% of this baseline on intended real workloads while adding substantial complexity, prefer COW. Grammar/recompression should only return to consideration if real traces expose a large, repeatable deficiency that conventional COW/content addressing cannot capture.

The incremental CDC backend remains useful for measuring content-defined dedup behavior, but its flat version manifests give it a metadata disadvantage relative to tree-based representations. Do not attribute that metadata gap to intrinsic CDC payload behavior.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
