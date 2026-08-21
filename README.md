# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1 — frozen result

The first controlled workload compared three representations under the same deterministic arbitrary-parent branch/edit/read plan:

- **persistent AVL byte rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, 4 KiB-style byte leaves, direct range traversal. Splitting a leaf copies the affected leaf fragments.
- **persistent COW piece rope** — persistent balanced-tree indexing whose leaves are slices of immutable shared buffers. Splitting a piece allocates metadata only; unchanged bytes are never copied.
- **incremental windowed CDC** — fixed-window rolling content-defined chunking with exact-byte deduplication and flat per-version manifests. Edits re-chunk only a repair region and reuse the untouched old suffix after exact boundary resynchronization.

At 1,000 branches over a 2 MiB base state (seed `0x5eed1234d15ca11e`, max edit 96 bytes, 4 KiB reads), all three backends produced the same checksum and sampled historical versions matched byte-for-byte.

The measured result is a **negative result for the AVL byte-rope candidate**:

| backend | retained growth / branch | edit p95 | read p95 |
| --- | ---: | ---: | ---: |
| persistent AVL byte rope | 4,841.7 B | 9.168 us | 3.496 us |
| persistent COW piece rope | **892.6 B** | **3.186 us** | **3.296 us** |
| incremental windowed CDC | 20,686.8 B | 327.548 us | 1.102 us |

On this workload, COW uses about **5.4x less retained growth** than the AVL byte rope, is about **2.9x faster at p95 edits**, and has essentially the same p95 read latency. Therefore the AVL byte rope is **dominated** and must not be promoted as a Tulya architecture or novelty claim.

The useful mechanism exposed by Phase 1 is ordinary structural persistence plus copy-on-write byte sharing. Any more elaborate grammar/recompression design now has to beat the COW baseline materially, not merely beat snapshots, naive deltas, or CDC.

### Quarantined early CDC results

Results produced before commit `2f4147cdc95b6e3347b10983a9f846990b7d2684` used a weaker chunk-prefix fingerprint that reset at every chunk boundary. Those runs are implementation diagnostics only and **must not be used as evidence that AVL beats a strong CDC baseline**.

## Phase 2 — workload falsification

The four deterministic workload families are:

| workload | purpose |
| --- | --- |
| `small-edit` | frozen Phase-1 arbitrary-parent insert/delete/replace baseline |
| `append-heavy` | agent/chat-style histories: mostly extend the latest version, sometimes fork from historical parents |
| `cross-template` | unrelated branches independently inject one of four identical large structured payloads; challenges ancestry-only COW with reusable content |
| `large-rewrite` | arbitrary historical parents replace large contiguous regions with fresh generated content |

The Phase-2 runs showed COW winning the small-edit and large-rewrite workloads and remaining competitive on append-heavy history. `cross-template`, deliberately constructed to favor global deduplication, was the only case where CDC used less retained storage: about 52.9 KiB/branch versus 67.8 KiB/branch for the original COW implementation, while CDC paid roughly two orders of magnitude more p95 edit latency.

That result does **not** justify grammar/recompression. The simpler next adversary is conventional COW plus exact-content interning of immutable backing buffers.

## COW + exact buffer interning

`persistent-piece-cow-interned` keeps the same persistent piece tree. The only change is backing-buffer allocation:

1. hash the candidate immutable buffer;
2. inspect only candidates with the same hash;
3. require exact byte-for-byte equality before reuse;
4. otherwise allocate a new `Arc<[u8]>`.

The hash is therefore only a lookup accelerator; collisions cannot cause false sharing. The interning index stores weak references and does not keep otherwise-dead buffers alive. Hash-table bucket/control overhead remains excluded from the representation-size estimate, consistently with the existing CDC accounting.

This follow-up has exactly two useful synthetic reruns:

```bash
cargo test --all-targets

cargo run --release -- \
  --workload cross-template \
  --branches 500 \
  --base-mib 2 \
  --edit-bytes 65536

cargo run --release -- \
  --workload large-rewrite \
  --branches 500 \
  --base-mib 8 \
  --edit-bytes 65536
```

If exact buffer interning erases the cross-template storage advantage while preserving COW's latency advantage, do **not** add another synthetic representation. Move directly to real repository/agent state traces.

If CDC still retains a large, credible storage advantage after this change, inspect why before considering any grammar/recompression design.

## What is measured

The harness reports build time, edit and range-read p50/p95/p99, retained payload and explicit metadata, retained growth per branch, lifetime representation allocation, live/lifetime structural objects, a read checksum, and sampled cross-backend semantic equality.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, process/runtime overhead, and temporary buffers are excluded. The lifetime-allocation metric therefore measures allocations retained by or created for the representation itself; it is not a full heap-allocation or physical-write-amplification measurement.

## Decision rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

The current baseline to beat is **persistent COW pieces with exact immutable-buffer interning**, not the AVL byte rope. If a more elaborate candidate comes within roughly 20–25% of this baseline on intended workloads while adding substantial complexity, prefer COW. A grammar/recompression candidate should only survive if it produces a large combined gain that remains after metadata, random/range reads, edit latency, and realistic state shapes are counted.

The incremental CDC backend remains useful for measuring content-defined dedup behavior, but its flat version manifests give it a metadata disadvantage relative to tree-based representations. Do not attribute that metadata gap to intrinsic CDC payload behavior.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
