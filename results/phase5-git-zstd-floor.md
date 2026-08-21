# Phase 5 — Git/zstd storage-floor result

Validated locally on the 24-case repository-diverse SWE-bench Verified corpus.

These are **storage-only floors**, not branch-native systems. They do not get credit for Tulya/COW historical edit, arbitrary-parent fork, or range-read semantics.

## Result

| baseline | retained bases | final storage | child growth / case |
| --- | ---: | ---: | ---: |
| raw independent snapshots | 440.599 MiB | 881.205 MiB | 19,250,357.4 B |
| zstd level 3, each snapshot independently | 142.006 MiB | 284.015 MiB | 6,204,466.1 B |
| Git aggressive pack/delta | **77.996 MiB** | **78.021 MiB** | **1,106.4 B** |

The Git result is the first conventional baseline that beats persistent COW by a large multiple on repository child storage. The corrected file-aware COW corpus result was 51,802.5 B/case, so Git's aggressively repacked child growth is about 46.8x smaller.

Git also retained the independent base roots much more compactly than the current COW and CDC prototypes: about 78 MiB versus about 451 MiB for COW and 252 MiB for CDC on this corpus.

## What this proves — and what it does not

The result proves that the repository corpus contains very large cross-version redundancy that a mature delta packer can exploit. It invalidates any claim that COW is close to the best achievable repository storage density.

It does **not** prove that Git's representation is a suitable online agent checkpoint substrate. The measured Git number is obtained after an aggressive whole-object-database repack. The current benchmark does not yet report:

- child object growth before repacking;
- child write latency before repacking;
- standard/non-aggressive repack size and time;
- aggressive repack time;
- historical object materialization latency;
- random/range-read behavior (Git normally reconstructs the full blob);
- arbitrary checkpoint edits without materializing a full new blob first.

Therefore the 1.1 KiB/case figure is a **compression floor**, not an online write-cost claim.

## Decision

Do not implement grammar/recompression yet. First decompose the Git result operationally.

1. Measure loose/incremental child-object growth before repack.
2. Measure standard repack storage and elapsed time.
3. Measure aggressive repack storage and elapsed time.
4. Measure exact historical child materialization latency after packing.
5. If delta packing remains compelling, add an xdelta/VCDIFF-style pairwise-delta floor to determine how much of Git's win comes from generic parent/nearby-object deltas versus Git-specific pack heuristics.

Brotli remains low priority. Independent zstd already shows that whole-snapshot compression alone is not the mechanism responsible for Git's win.
