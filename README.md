# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1

The experiment now compares three representations under the same deterministic branch/edit/read plan:

- **persistent AVL byte rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, 4 KiB-style byte leaves, direct range traversal. Splitting a leaf copies the affected leaf fragments.
- **persistent COW piece rope** — the same broad persistent balanced-tree indexing strategy, but leaves are slices of immutable shared buffers. Splitting a piece allocates metadata only; unchanged bytes are never copied and each inserted payload is allocated once. This is the strongest simple adversary to the current byte-rope design.
- **incremental windowed CDC** — fixed-window rolling content-defined chunking with exact-byte deduplication and flat per-version manifests. Edits begin at an affected old chunk boundary, re-chunk a bounded repair region, and reuse the untouched old suffix after exact boundary resynchronization. The manifest itself is still flat and therefore not structurally shared across versions.

Every generated child chooses an arbitrary historical parent. All backends must retain all version handles. Sampled versions are fully decoded and compared byte-for-byte after timing.

The repository intentionally excludes durability/WAL, networking, auth, crypto, framework integrations, production APIs, and Lean↔Rust refinement. Those only become relevant if a representation survives the benchmark.

### Quarantined pre-windowed results

Results produced before commit `2f4147cdc95b6e3347b10983a9f846990b7d2684` used a weaker chunk-prefix fingerprint that reset at every chunk boundary. Those runs are useful as implementation diagnostics but **must not be used as evidence that AVL beats a strong CDC baseline**, because a small insertion/deletion could perturb boundary phase far into the suffix.

## What is measured

The harness reports:

- build time;
- edit latency p50/p95/p99;
- range-read latency p50/p95/p99;
- retained payload bytes;
- retained explicit metadata bytes;
- retained growth per branch;
- lifetime **representation** allocation per branch;
- live versus lifetime-allocated structural objects;
- a read checksum plus sampled cross-backend semantic equality.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, process/runtime overhead, and temporary buffers are excluded. The lifetime-allocation metric therefore measures allocations retained by or created for the representation itself; it is not a full heap-allocation or physical-write-amplification measurement.

## Kill rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

The COW piece rope is intentionally dangerous to the current thesis. If it matches or beats the AVL byte rope on retained growth and latency, then the useful mechanism is ordinary structural persistence plus copy-on-write byte sharing, not a special compressed representation. In that case the AVL byte-rope result should be treated as dominated, not promoted.

Likewise, if a simple COW/piece representation comes within roughly 20–25% of any more elaborate candidate on the intended workloads with much less complexity, prefer the simpler design. A later grammar/recompression candidate must beat this COW baseline materially to justify its complexity.

The incremental CDC backend remains useful for measuring content-defined dedup behavior, but its flat version manifests give it a metadata disadvantage relative to the tree-based representations. Do not attribute that metadata gap to intrinsic CDC payload behavior.

## Run locally

Requires stable Rust. There are no third-party Rust dependencies.

```bash
cargo test --all-targets
cargo run --release -- --branches 128 --base-kib 256 --verify-samples 12
cargo run --release -- --branches 1000 --base-mib 2
```

Do not scale to the 10k workload until the three-way 1k comparison is healthy and understood. Use `cargo run --release -- --help` for all options.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
