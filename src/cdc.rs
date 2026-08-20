use crate::model::{Backend, BackendStats, Edit, StateError};
use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, Weak};

const CDC_WINDOW: usize = 64;
const REPAIR_MAX_MULTIPLIER: usize = 4;

#[derive(Clone, Copy)]
struct ChunkRef {
    id: usize,
    start: usize,
}

#[derive(Clone)]
pub struct Snapshot {
    chunks: Arc<[ChunkRef]>,
    len: usize,
}

struct TrackedManifest {
    weak: Weak<[ChunkRef]>,
    bytes: usize,
}

pub struct CdcStore {
    min_chunk: usize,
    avg_chunk: usize,
    max_chunk: usize,
    boundary_mask: u64,
    chunks: Vec<Arc<[u8]>>,
    index: HashMap<u64, Vec<usize>>,
    manifests: Vec<TrackedManifest>,
    lifetime_payload_bytes: usize,
    lifetime_metadata_bytes: usize,
}

impl CdcStore {
    pub fn new(avg_chunk: usize) -> Self {
        assert!(avg_chunk >= 64, "avg_chunk must be at least 64 bytes");
        let avg_power = avg_chunk.next_power_of_two();
        Self {
            min_chunk: (avg_chunk / 4).max(32),
            avg_chunk,
            max_chunk: avg_chunk.saturating_mul(4),
            boundary_mask: (avg_power as u64).saturating_sub(1),
            chunks: Vec::new(),
            index: HashMap::new(),
            manifests: Vec::new(),
            lifetime_payload_bytes: 0,
            lifetime_metadata_bytes: 0,
        }
    }

    pub fn avg_chunk(&self) -> usize {
        self.avg_chunk
    }

    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    fn gear(byte: u8) -> u64 {
        Self::splitmix64(byte as u64 + 0x517c_c1b7_2722_0a95)
    }

    fn hash_bytes(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Split using a 64-byte rolling buzhash-style fingerprint.
    ///
    /// The fingerprint is not reset at chunk boundaries. Once an insertion or
    /// deletion is more than one window behind us, the fingerprint again
    /// depends only on the local byte window. This lets an incremental edit
    /// detect a common boundary with the unchanged old suffix and reuse all
    /// chunks after that point.
    fn chunk_ranges(&self, bytes: &[u8]) -> Vec<(usize, usize)> {
        if bytes.is_empty() {
            return Vec::new();
        }

        let mut ranges = Vec::new();
        let mut start = 0usize;
        let mut hash = 0u64;
        let mut window = [0u8; CDC_WINDOW];
        let mut filled = 0usize;
        let outgoing_rotation = (CDC_WINDOW % u64::BITS as usize) as u32;

        for (i, &byte) in bytes.iter().enumerate() {
            if filled < CDC_WINDOW {
                window[filled] = byte;
                filled += 1;
                hash = hash.rotate_left(1) ^ Self::gear(byte);
            } else {
                let slot = i % CDC_WINDOW;
                let outgoing = window[slot];
                window[slot] = byte;
                hash = hash.rotate_left(1)
                    ^ Self::gear(byte)
                    ^ Self::gear(outgoing).rotate_left(outgoing_rotation);
            }

            let chunk_len = i + 1 - start;
            let fingerprint_boundary =
                filled == CDC_WINDOW && (hash & self.boundary_mask) == 0;
            let boundary = chunk_len >= self.min_chunk
                && (fingerprint_boundary || chunk_len >= self.max_chunk);
            if boundary {
                ranges.push((start, i + 1));
                start = i + 1;
            }
        }
        if start < bytes.len() {
            ranges.push((start, bytes.len()));
        }
        ranges
    }

    fn intern(&mut self, bytes: &[u8]) -> usize {
        let hash = Self::hash_bytes(bytes);
        if let Some(candidates) = self.index.get(&hash) {
            for &id in candidates {
                if self.chunks[id].as_ref() == bytes {
                    return id;
                }
            }
        }

        let id = self.chunks.len();
        let chunk: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        self.lifetime_payload_bytes = self
            .lifetime_payload_bytes
            .saturating_add(chunk.len());
        self.lifetime_metadata_bytes = self
            .lifetime_metadata_bytes
            .saturating_add(size_of::<Arc<[u8]>>());
        self.chunks.push(chunk);
        self.index.entry(hash).or_default().push(id);
        id
    }

    fn finish_snapshot(&mut self, refs: Vec<ChunkRef>, len: usize) -> Snapshot {
        let chunks: Arc<[ChunkRef]> = Arc::from(refs.into_boxed_slice());
        let manifest_bytes = chunks.len().saturating_mul(size_of::<ChunkRef>());
        self.lifetime_metadata_bytes = self
            .lifetime_metadata_bytes
            .saturating_add(manifest_bytes);
        self.manifests.push(TrackedManifest {
            weak: Arc::downgrade(&chunks),
            bytes: manifest_bytes,
        });
        Snapshot { chunks, len }
    }

    fn snapshot_from_bytes(&mut self, bytes: &[u8]) -> Snapshot {
        let mut refs = Vec::new();
        for (start, end) in self.chunk_ranges(bytes) {
            let id = self.intern(&bytes[start..end]);
            refs.push(ChunkRef { id, start });
        }
        self.finish_snapshot(refs, bytes.len())
    }

    fn first_chunk_for_offset(&self, snapshot: &Snapshot, offset: usize) -> usize {
        let mut lo = 0usize;
        let mut hi = snapshot.chunks.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = snapshot.chunks[mid];
            let end = entry.start + self.chunks[entry.id].len();
            if end <= offset {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn chunk_index_starting_at(&self, snapshot: &Snapshot, start: usize) -> Option<usize> {
        snapshot
            .chunks
            .binary_search_by_key(&start, |entry| entry.start)
            .ok()
    }

    fn rounded_scan_end(&self, snapshot: &Snapshot, target: usize) -> usize {
        if target >= snapshot.len {
            return snapshot.len;
        }
        let index = self.first_chunk_for_offset(snapshot, target);
        if let Some(entry) = snapshot.chunks.get(index).copied() {
            entry
                .start
                .saturating_add(self.chunks[entry.id].len())
                .min(snapshot.len)
        } else {
            snapshot.len
        }
    }

    /// Incrementally repair the chunking around one edit.
    ///
    /// We begin at an existing old chunk boundary at or before the edit, scan a
    /// bounded unchanged suffix, and look for a newly generated natural chunk
    /// boundary that maps exactly to an old suffix boundary. Once such a common
    /// boundary is found, all later old chunk IDs can be reused directly. If a
    /// bounded repair fails to resynchronize, correctness wins: we fall back to
    /// canonical full-state re-chunking for that edit.
    fn incremental_edit(
        &mut self,
        parent: &Snapshot,
        edit: &Edit,
    ) -> Result<Snapshot, StateError> {
        edit.validate_len(parent.len)?;
        let child_len = edit.output_len(parent.len)?;

        if parent.chunks.is_empty() {
            return Ok(self.snapshot_from_bytes(&edit.insert));
        }

        let old_edit_end = edit
            .start
            .checked_add(edit.delete_len)
            .ok_or(StateError::LengthOverflow)?;
        let new_edit_end = edit
            .start
            .checked_add(edit.insert.len())
            .ok_or(StateError::LengthOverflow)?;

        let mut anchor_index = self.first_chunk_for_offset(parent, edit.start);
        if anchor_index >= parent.chunks.len() {
            anchor_index = parent.chunks.len() - 1;
        }
        let anchor_start = parent.chunks[anchor_index].start;

        let repair_budget = self
            .max_chunk
            .saturating_mul(REPAIR_MAX_MULTIPLIER)
            .saturating_add(CDC_WINDOW);
        let scan_target = old_edit_end.saturating_add(repair_budget).min(parent.len);
        let scan_old_end = self.rounded_scan_end(parent, scan_target);

        let old_local = self.read_range(parent, anchor_start, scan_old_end - anchor_start)?;
        let local_edit = Edit {
            start: edit.start - anchor_start,
            delete_len: edit.delete_len,
            insert: edit.insert.clone(),
        };
        let child_local = local_edit.apply(&old_local)?;
        let ranges = self.chunk_ranges(&child_local);

        let min_resync_new = new_edit_end.saturating_add(CDC_WINDOW);
        let mut resync = None;

        if scan_old_end < parent.len {
            // The final range may exist only because the repair slice ended, so
            // only earlier range ends are guaranteed natural CDC boundaries.
            for (range_index, &(_, local_end)) in ranges
                .iter()
                .enumerate()
                .take(ranges.len().saturating_sub(1))
            {
                let new_boundary = anchor_start.saturating_add(local_end);
                if new_boundary < min_resync_new || new_boundary < new_edit_end {
                    continue;
                }
                let suffix_distance = new_boundary - new_edit_end;
                let old_boundary = old_edit_end.saturating_add(suffix_distance);
                if old_boundary > scan_old_end {
                    continue;
                }
                if let Some(suffix_index) = self.chunk_index_starting_at(parent, old_boundary) {
                    resync = Some((range_index + 1, old_boundary, suffix_index));
                    break;
                }
            }
        }

        if scan_old_end < parent.len && resync.is_none() {
            let parent_bytes = self.read_all(parent)?;
            let child = edit.apply(&parent_bytes)?;
            return Ok(self.snapshot_from_bytes(&child));
        }

        let mut refs = Vec::with_capacity(parent.chunks.len().saturating_add(8));
        refs.extend_from_slice(&parent.chunks[..anchor_index]);

        match resync {
            Some((middle_count, old_boundary, suffix_index)) => {
                for &(start, end) in ranges.iter().take(middle_count) {
                    let id = self.intern(&child_local[start..end]);
                    refs.push(ChunkRef {
                        id,
                        start: anchor_start + start,
                    });
                }

                debug_assert_eq!(parent.chunks[suffix_index].start, old_boundary);
                for entry in parent.chunks[suffix_index..].iter().copied() {
                    let suffix_distance = entry.start - old_edit_end;
                    refs.push(ChunkRef {
                        id: entry.id,
                        start: new_edit_end + suffix_distance,
                    });
                }
            }
            None => {
                // The repair slice reached EOF, so it already contains the full
                // child suffix and there is nothing old left to append.
                for &(start, end) in &ranges {
                    let id = self.intern(&child_local[start..end]);
                    refs.push(ChunkRef {
                        id,
                        start: anchor_start + start,
                    });
                }
            }
        }

        Ok(self.finish_snapshot(refs, child_len))
    }
}

impl Backend for CdcStore {
    type Snapshot = Snapshot;

    fn name(&self) -> &'static str {
        "incremental-windowed-cdc"
    }

    fn create(&mut self, bytes: &[u8]) -> Self::Snapshot {
        self.snapshot_from_bytes(bytes)
    }

    fn len(&self, snapshot: &Self::Snapshot) -> usize {
        snapshot.len
    }

    fn read_range(
        &self,
        snapshot: &Self::Snapshot,
        start: usize,
        len: usize,
    ) -> Result<Vec<u8>, StateError> {
        let end = start.checked_add(len).ok_or(StateError::LengthOverflow)?;
        if start > snapshot.len || end > snapshot.len {
            return Err(StateError::OutOfBounds {
                len: snapshot.len,
                start,
                delete_len: len,
            });
        }
        if len == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(len);
        let mut index = self.first_chunk_for_offset(snapshot, start);
        let mut cursor = start;
        while cursor < end {
            let entry = *snapshot
                .chunks
                .get(index)
                .ok_or(StateError::Corrupt("CDC manifest ended early"))?;
            let chunk = self
                .chunks
                .get(entry.id)
                .ok_or(StateError::Corrupt("CDC manifest references missing chunk"))?;
            let local_start = cursor.saturating_sub(entry.start);
            if local_start >= chunk.len() {
                return Err(StateError::Corrupt("CDC chunk offset is inconsistent"));
            }
            let take = (end - cursor).min(chunk.len() - local_start);
            out.extend_from_slice(&chunk[local_start..local_start + take]);
            cursor += take;
            index += 1;
        }
        Ok(out)
    }

    fn edit(
        &mut self,
        parent: &Self::Snapshot,
        edit: &Edit,
    ) -> Result<Self::Snapshot, StateError> {
        self.incremental_edit(parent, edit)
    }

    fn validate(&self, snapshot: &Self::Snapshot) -> Result<(), String> {
        let mut expected_start = 0usize;
        for entry in snapshot.chunks.iter() {
            if entry.start != expected_start {
                return Err(format!(
                    "CDC manifest gap/overlap: expected start {expected_start}, got {}",
                    entry.start
                ));
            }
            let chunk = self
                .chunks
                .get(entry.id)
                .ok_or_else(|| "CDC manifest references missing chunk".to_string())?;
            expected_start = expected_start
                .checked_add(chunk.len())
                .ok_or_else(|| "CDC manifest length overflow".to_string())?;
        }
        if expected_start != snapshot.len {
            return Err(format!(
                "CDC manifest length mismatch: manifest={expected_start}, snapshot={} ",
                snapshot.len
            ));
        }
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        let retained_payload_bytes = self.chunks.iter().map(|c| c.len()).sum();
        let chunk_metadata = self
            .chunks
            .len()
            .saturating_mul(size_of::<Arc<[u8]>>());
        let mut manifest_metadata = 0usize;
        let mut live_manifests = 0usize;
        for manifest in &self.manifests {
            if manifest.weak.upgrade().is_some() {
                live_manifests += 1;
                manifest_metadata = manifest_metadata.saturating_add(manifest.bytes);
            }
        }
        BackendStats {
            retained_payload_bytes,
            retained_metadata_bytes: chunk_metadata.saturating_add(manifest_metadata),
            lifetime_payload_bytes: self.lifetime_payload_bytes,
            lifetime_metadata_bytes: self.lifetime_metadata_bytes,
            live_objects: self.chunks.len().saturating_add(live_manifests),
            total_objects_allocated: self.chunks.len().saturating_add(self.manifests.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_bytes(len: usize) -> Vec<u8> {
        let mut x = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn shifted_insert_is_exact_and_parent_is_unchanged() {
        let mut cdc = CdcStore::new(128);
        let base_bytes: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let base = cdc.create(&base_bytes);
        let edit = Edit {
            start: 777,
            delete_len: 3,
            insert: b"hello-content-defined-world".to_vec(),
        };
        let expected = edit.apply(&base_bytes).unwrap();
        let child = cdc.edit(&base, &edit).unwrap();

        assert_eq!(cdc.read_all(&base).unwrap(), base_bytes);
        assert_eq!(cdc.read_all(&child).unwrap(), expected);
        cdc.validate(&base).unwrap();
        cdc.validate(&child).unwrap();
    }

    #[test]
    fn exact_dedup_reuses_identical_base() {
        let mut cdc = CdcStore::new(128);
        let bytes = vec![b'x'; 8192];
        let first = cdc.create(&bytes);
        let before = cdc.stats().retained_payload_bytes;
        let second = cdc.create(&bytes);
        let after = cdc.stats().retained_payload_bytes;
        assert_eq!(before, after);
        assert_eq!(cdc.read_all(&first).unwrap(), cdc.read_all(&second).unwrap());
    }

    #[test]
    fn small_insert_resynchronizes_and_reuses_most_payload() {
        let mut cdc = CdcStore::new(4096);
        let bytes = deterministic_bytes(1024 * 1024);
        let base = cdc.create(&bytes);
        let before = cdc.stats().retained_payload_bytes;
        let edit = Edit {
            start: 128 * 1024 + 17,
            delete_len: 0,
            insert: b"localized insertion".to_vec(),
        };
        let expected = edit.apply(&bytes).unwrap();
        let child = cdc.edit(&base, &edit).unwrap();
        let after = cdc.stats().retained_payload_bytes;

        assert_eq!(cdc.read_all(&child).unwrap(), expected);
        assert!(
            after.saturating_sub(before) < 64 * 1024,
            "localized edit created too much new chunk payload: {} bytes",
            after.saturating_sub(before)
        );
    }

    #[test]
    fn repeated_historical_branching_stays_exact() {
        let mut cdc = CdcStore::new(4096);
        let base_bytes = deterministic_bytes(256 * 1024);
        let base = cdc.create(&base_bytes);
        let mut snapshots = vec![base];
        let mut expected = vec![base_bytes];

        for i in 0..128usize {
            let parent = (i * 37) % snapshots.len();
            let parent_len = expected[parent].len();
            let start = (i * 7919) % (parent_len + 1);
            let delete_len = (i % 31).min(parent_len - start);
            let insert = vec![(i & 0xff) as u8; (i * 13) % 47];
            let edit = Edit {
                start,
                delete_len,
                insert,
            };
            let child_expected = edit.apply(&expected[parent]).unwrap();
            let child = cdc.edit(&snapshots[parent], &edit).unwrap();
            assert_eq!(cdc.read_all(&child).unwrap(), child_expected);
            cdc.validate(&child).unwrap();
            snapshots.push(child);
            expected.push(child_expected);
        }

        assert_eq!(cdc.read_all(&snapshots[0]).unwrap(), expected[0]);
    }
}
