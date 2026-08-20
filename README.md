# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1 — frozen result

The first controlled workload compared three representations under the same deterministic arbitrary-parent branch/edit/read plan:

- **persistent AVL byte rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, 4 KiB-style byte leaves, direct range traversal. Splitting a leaf copies the affected leaf fragments.
- **persistent COW piece rope** — persistent balanced-tree indexing whose leaves are slices of immutable shared buffers. Splitting a piece allocates metadata only; unchanged bytes are never copied and each inserted payload is allocated once.
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

Do not treat this synthetic small-edit workload as proof that COW is the final product architecture. It is specifically favorable to ancestry-local sharing. The next phase must test workload shapes where extra compression could plausibly add value: append-heavy histories, repeated/template content across unrelated branches, large replacements, and real repository/agent state traces. We should not implement a grammar/recompression engine until those workloads show a measurable gap that COW cannot capture.

### Quarantined early CDC results

Results produced before commit `2f4147cdc95b6e3347b10983a9f846990b7d2684` used a weaker chunk-prefix fingerprint that reset at every chunk boundary. Those runs are implementation diagnostics only and **must not be used as evidence that AVL beats a strong CDC baseline**.

## What is measured

The harness reports build time, edit and range-read p50/p95/p99, retained payload and explicit metadata, retained growth per branch, lifetime representation allocation, live/lifetime structural objects, a read checksum, and sampled cross-backend semantic equality.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, process/runtime overhead, and temporary buffers are excluded. The lifetime-allocation metric therefore measures allocations retained by or created for the representation itself; it is not a full heap-allocation or physical-write-amplification measurement.

## Decision rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

The current baseline to beat is **persistent COW pieces**, not the AVL byte rope. If a more elaborate candidate comes within roughly 20–25% of COW on intended workloads while adding substantial complexity, prefer COW. A grammar/recompression candidate should only survive if it produces a large combined gain that remains after metadata, random/range reads, edit latency, and realistic state shapes are counted.

The incremental CDC backend remains useful for measuring content-defined dedup behavior, but its flat version manifests give it a metadata disadvantage relative to tree-based representations. Do not attribute that metadata gap to intrinsic CDC payload behavior.

## Run locally

Requires stable Rust. There are no third-party Rust dependencies.

```bash
cargo test --all-targets
cargo run --release -- --branches 128 --base-kib 256 --verify-samples 12
cargo run --release -- --branches 1000 --base-mib 2
```

Do not scale this exact synthetic workload to 10k merely to amplify a result we already understand. The next useful work is workload diversification, not branch-count inflation.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
