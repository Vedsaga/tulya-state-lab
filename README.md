# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

The first experiment compares:

- a functional persistent AVL rope with path-copying and chunked leaves;
- a simple rolling-hash content-defined-chunking (CDC) store with exact-byte deduplication.

The repository intentionally excludes durability/WAL, networking, auth, crypto, framework integrations, production APIs, and Lean↔Rust refinement. Those only become relevant if a representation survives the benchmark.

## Phase-1 gate

We care about arbitrary historical branching, small insert/delete/replace edits, random/range reads, retained physical bytes, allocation/write amplification, and latency. A representation is not interesting merely because it is elegant or formally provable.

The initial kill rule is deliberately harsh: if the persistent structure cannot show a large combined advantage on branch-heavy workloads over the simple CDC adversary—or if that advantage disappears once metadata and read costs are counted—we should not promote it into an engine design.

Build/run instructions will be added with the phase-1 harness.
