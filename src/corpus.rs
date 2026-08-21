use crate::model::{Backend, BackendStats, Edit, StateError};
use crate::workload::LatencySummary;
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const PACK_MAGIC: &[u8] = b"TULYA_REPO_PACK_V1\0";
const PACK_ENTRY_HEADER_BYTES: usize = 1 + 4 + 4 + 8;

#[derive(Clone, Debug)]
pub struct CorpusCase {
    pub id: String,
    pub base: Vec<u8>,
    pub child_len: usize,
    pub edits: Vec<Edit>,
    pub read_start: usize,
    pub read_len: usize,
}

#[derive(Clone, Debug)]
pub struct Corpus {
    pub cases: Vec<CorpusCase>,
    pub logical_bytes: u128,
}

#[derive(Clone, Debug)]
pub struct CorpusReport {
    pub backend: &'static str,
    pub base_build_ns: u64,
    pub edit: LatencySummary,
    pub read: LatencySummary,
    pub base_stats: BackendStats,
    pub final_stats: BackendStats,
    pub checksum: u64,
    pub cases: usize,
    pub edit_hunks: usize,
    pub logical_bytes: u128,
}

#[derive(Clone, Debug)]
pub struct CorpusOutcome {
    pub report: CorpusReport,
    pub sample_indices: Vec<usize>,
    pub sample_children: Vec<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct PackedEntry {
    path: Vec<u8>,
    start: usize,
    end: usize,
}

fn read_u32_le(bytes: &[u8], start: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(start..start.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn read_u64_le(bytes: &[u8], start: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(start..start.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

fn parse_pack(bytes: &[u8]) -> Option<Vec<PackedEntry>> {
    if !bytes.starts_with(PACK_MAGIC) {
        return None;
    }
    let mut cursor = PACK_MAGIC.len();
    let count = usize::try_from(read_u64_le(bytes, cursor)?).ok()?;
    cursor = cursor.checked_add(8)?;
    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        let start = cursor;
        let header_end = cursor.checked_add(PACK_ENTRY_HEADER_BYTES)?;
        bytes.get(cursor..header_end)?;
        let path_len = usize::try_from(read_u32_le(bytes, cursor + 5)?).ok()?;
        let content_len = usize::try_from(read_u64_le(bytes, cursor + 9)?).ok()?;
        cursor = header_end;
        let path_end = cursor.checked_add(path_len)?;
        let path = bytes.get(cursor..path_end)?.to_vec();
        let end = path_end.checked_add(content_len)?;
        bytes.get(path_end..end)?;
        entries.push(PackedEntry { path, start, end });
        cursor = end;
    }

    if cursor != bytes.len() {
        return None;
    }
    Some(entries)
}

fn derive_local_edit(base: &[u8], child: &[u8], base_offset: usize) -> Option<Edit> {
    if base == child {
        return None;
    }
    let common_limit = base.len().min(child.len());
    let mut prefix = 0usize;
    while prefix < common_limit && base[prefix] == child[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    let base_remaining = base.len().saturating_sub(prefix);
    let child_remaining = child.len().saturating_sub(prefix);
    let suffix_limit = base_remaining.min(child_remaining);
    while suffix < suffix_limit
        && base[base.len() - 1 - suffix] == child[child.len() - 1 - suffix]
    {
        suffix += 1;
    }

    Some(Edit {
        start: base_offset + prefix,
        delete_len: base.len() - prefix - suffix,
        insert: child[prefix..child.len() - suffix].to_vec(),
    })
}

fn pack_entries_start() -> usize {
    PACK_MAGIC.len() + 8
}

fn derive_pack_edits(base: &[u8], child: &[u8]) -> Option<Vec<Edit>> {
    let base_entries = parse_pack(base)?;
    let child_entries = parse_pack(child)?;
    let entries_start = pack_entries_start();
    if base.len() < entries_start || child.len() < entries_start {
        return None;
    }

    let mut common = Vec::new();
    let (mut bi, mut ci) = (0usize, 0usize);
    while bi < base_entries.len() && ci < child_entries.len() {
        match base_entries[bi].path.cmp(&child_entries[ci].path) {
            Ordering::Less => bi += 1,
            Ordering::Greater => ci += 1,
            Ordering::Equal => {
                common.push((bi, ci));
                bi += 1;
                ci += 1;
            }
        }
    }

    let mut edits = Vec::new();
    if let Some(edit) = derive_local_edit(
        &base[PACK_MAGIC.len()..entries_start],
        &child[PACK_MAGIC.len()..entries_start],
        PACK_MAGIC.len(),
    ) {
        edits.push(edit);
    }

    let mut prev_b = 0usize;
    let mut prev_c = 0usize;
    for &(common_b, common_c) in &common {
        if prev_b < common_b || prev_c < common_c {
            let base_start = if prev_b < base_entries.len() {
                base_entries[prev_b].start
            } else {
                base.len()
            };
            let base_end = if prev_b < common_b {
                base_entries[common_b - 1].end
            } else {
                base_start
            };
            let child_start = if prev_c < child_entries.len() {
                child_entries[prev_c].start
            } else {
                child.len()
            };
            let child_end = if prev_c < common_c {
                child_entries[common_c - 1].end
            } else {
                child_start
            };
            edits.push(Edit {
                start: base_start,
                delete_len: base_end - base_start,
                insert: child[child_start..child_end].to_vec(),
            });
        }

        let base_entry = &base_entries[common_b];
        let child_entry = &child_entries[common_c];
        if let Some(edit) = derive_local_edit(
            &base[base_entry.start..base_entry.end],
            &child[child_entry.start..child_entry.end],
            base_entry.start,
        ) {
            edits.push(edit);
        }
        prev_b = common_b + 1;
        prev_c = common_c + 1;
    }

    if prev_b < base_entries.len() || prev_c < child_entries.len() {
        let base_start = if prev_b < base_entries.len() {
            base_entries[prev_b].start
        } else {
            base.len()
        };
        let base_end = base_entries.last().map_or(base_start, |entry| entry.end);
        let child_start = if prev_c < child_entries.len() {
            child_entries[prev_c].start
        } else {
            child.len()
        };
        let child_end = child_entries.last().map_or(child_start, |entry| entry.end);
        edits.push(Edit {
            start: base_start,
            delete_len: base_end - base_start,
            insert: child[child_start..child_end].to_vec(),
        });
    }

    // Every edit is expressed in coordinates of the original base. Applying
    // high offsets first keeps all remaining lower offsets valid. At an equal
    // offset, replacements/deletions must happen before a pure insertion so an
    // inserted run remains before the modified entry that originally followed it.
    edits.sort_by(|left, right| {
        right
            .start
            .cmp(&left.start)
            .then_with(|| right.delete_len.cmp(&left.delete_len))
    });
    Some(edits)
}

fn derive_edits(base: &[u8], child: &[u8]) -> Vec<Edit> {
    if let Some(edits) = derive_pack_edits(base, child) {
        return edits;
    }
    derive_local_edit(base, child, 0).into_iter().collect()
}

fn apply_edits(base: &[u8], edits: &[Edit]) -> Result<Vec<u8>, StateError> {
    let mut bytes = base.to_vec();
    for edit in edits {
        bytes = edit.apply(&bytes)?;
    }
    Ok(bytes)
}

fn checksum_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn id_hash(id: &str) -> u64 {
    checksum_bytes(0xcbf2_9ce4_8422_2325u64, id.as_bytes())
}

fn resolve_path(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn sample_indices(total: usize, verify_samples: usize) -> Vec<usize> {
    let count = verify_samples.max(2).min(total);
    (0..count)
        .map(|sample| {
            if sample + 1 == count {
                total - 1
            } else {
                sample.saturating_mul(total - 1) / (count - 1)
            }
        })
        .collect()
}

impl Corpus {
    /// Load a tab-separated manifest with at least three columns:
    ///
    /// `case_id<TAB>base_snapshot_path<TAB>child_snapshot_path`
    ///
    /// Extra columns are ignored so dataset metadata can travel with the trace.
    /// Repository packs produced by `prepare_swebench_verified.py` are diffed
    /// file-by-file; unknown binary snapshot formats fall back to one exact
    /// longest-prefix/suffix contiguous replacement.
    pub fn load_manifest(path: &Path, read_bytes: usize) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("failed to read corpus manifest {}: {e}", path.display()))?;
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        let mut cases = Vec::new();
        let mut logical_bytes = 0u128;

        for (line_no, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 3 {
                return Err(format!(
                    "manifest line {} must contain case_id, base path, and child path",
                    line_no + 1
                ));
            }
            let id = fields[0].to_string();
            let base_path = resolve_path(root, fields[1]);
            let child_path = resolve_path(root, fields[2]);
            let base = fs::read(&base_path).map_err(|e| {
                format!(
                    "failed to read base snapshot for {id} ({}): {e}",
                    base_path.display()
                )
            })?;
            let child = fs::read(&child_path).map_err(|e| {
                format!(
                    "failed to read child snapshot for {id} ({}): {e}",
                    child_path.display()
                )
            })?;
            let child_len = child.len();
            let edits = derive_edits(&base, &child);
            let rebuilt = apply_edits(&base, &edits)
                .map_err(|e| format!("derived edit script for {id} is invalid: {e}"))?;
            if rebuilt != child {
                return Err(format!("derived edit script for {id} does not reconstruct child"));
            }

            let read_len = read_bytes.min(child_len);
            let read_start = if read_len == 0 {
                0
            } else {
                (id_hash(&id) as usize) % (child_len - read_len + 1)
            };
            logical_bytes = logical_bytes
                .saturating_add(base.len() as u128)
                .saturating_add(child_len as u128);
            cases.push(CorpusCase {
                id,
                base,
                child_len,
                edits,
                read_start,
                read_len,
            });
        }

        if cases.is_empty() {
            return Err("corpus manifest contains no cases".into());
        }
        Ok(Self {
            cases,
            logical_bytes,
        })
    }

    pub fn edit_hunks(&self) -> usize {
        self.cases.iter().map(|case| case.edits.len()).sum()
    }
}

fn percentile(values: &[u64], numerator: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = (sorted.len() - 1).saturating_mul(numerator) / 100;
    sorted[idx]
}

fn summarize(values: &[u64]) -> LatencySummary {
    LatencySummary {
        p50_ns: percentile(values, 50),
        p95_ns: percentile(values, 95),
        p99_ns: percentile(values, 99),
    }
}

pub fn run_corpus_backend<B: Backend>(
    mut backend: B,
    corpus: &Corpus,
    verify_samples: usize,
) -> Result<CorpusOutcome, StateError> {
    let base_build_start = Instant::now();
    let mut bases = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        bases.push(backend.create(&case.base));
    }
    let base_build_ns = base_build_start
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let base_stats = backend.stats();

    let mut children = Vec::with_capacity(corpus.cases.len());
    let mut edit_latencies = Vec::with_capacity(corpus.cases.len());
    let mut read_latencies = Vec::with_capacity(corpus.cases.len());
    let mut checksum = 0xcbf2_9ce4_8422_2325u64;

    for (case, base) in corpus.cases.iter().zip(&bases) {
        let edit_start = Instant::now();
        let mut child = base.clone();
        for edit in &case.edits {
            child = backend.edit(&child, edit)?;
        }
        edit_latencies.push(edit_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        if backend.len(&child) != case.child_len {
            return Err(StateError::Corrupt("corpus edit script produced wrong child length"));
        }

        let read_start = Instant::now();
        let bytes = backend.read_range(&child, case.read_start, case.read_len)?;
        read_latencies.push(read_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        checksum = checksum_bytes(checksum, &bytes);
        children.push(child);
    }

    let final_stats = backend.stats();
    let selected = sample_indices(corpus.cases.len(), verify_samples);
    let mut sample_children = Vec::with_capacity(selected.len());
    for &index in &selected {
        backend
            .validate(&children[index])
            .map_err(|_| StateError::Corrupt("corpus backend validation failed"))?;
        sample_children.push(backend.read_all(&children[index])?);
    }

    let report = CorpusReport {
        backend: backend.name(),
        base_build_ns,
        edit: summarize(&edit_latencies),
        read: summarize(&read_latencies),
        base_stats,
        final_stats,
        checksum,
        cases: corpus.cases.len(),
        edit_hunks: corpus.edit_hunks(),
        logical_bytes: corpus.logical_bytes,
    };
    Ok(CorpusOutcome {
        report,
        sample_indices: selected,
        sample_children,
    })
}

pub fn verify_corpus_outcomes(
    left: &CorpusOutcome,
    right: &CorpusOutcome,
    corpus: &Corpus,
) -> Result<(), String> {
    if left.sample_indices != right.sample_indices
        || left.sample_children.len() != left.sample_indices.len()
        || right.sample_children.len() != right.sample_indices.len()
    {
        return Err("corpus verification sample sets differ".into());
    }

    for (sample_pos, &index) in left.sample_indices.iter().enumerate() {
        let case = &corpus.cases[index];
        let expected = apply_edits(&case.base, &case.edits)
            .map_err(|e| format!("failed to reconstruct expected child {}: {e}", case.id))?;
        let left_bytes = &left.sample_children[sample_pos];
        let right_bytes = &right.sample_children[sample_pos];
        if left_bytes != &expected || right_bytes != &expected || left_bytes != right_bytes {
            return Err(format!("corpus semantic mismatch at case {}", case.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(PACK_MAGIC);
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for &(path, content) in entries {
            out.push(1);
            out.extend_from_slice(&0o100644u32.to_le_bytes());
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u64).to_le_bytes());
            out.extend_from_slice(path);
            out.extend_from_slice(content);
        }
        out
    }

    #[test]
    fn contiguous_fallback_reconstructs_arbitrary_pair() {
        let base = b"alpha-OLD-middle-tail";
        let child = b"alpha-NEW-and-longer-middle-tail";
        let edits = derive_edits(base, child);
        assert_eq!(edits.len(), 1);
        assert_eq!(apply_edits(base, &edits).unwrap(), child);
    }

    #[test]
    fn identical_inputs_need_no_edits() {
        let bytes = b"same bytes";
        let edits = derive_edits(bytes, bytes);
        assert!(edits.is_empty());
        assert_eq!(apply_edits(bytes, &edits).unwrap(), bytes);
    }

    #[test]
    fn pack_diff_keeps_distant_file_changes_separate() {
        let base = pack(&[
            (b"a.txt", b"alpha OLD tail"),
            (b"middle.bin", &[b'x'; 128]),
            (b"z.txt", b"omega OLD tail"),
        ]);
        let child = pack(&[
            (b"a.txt", b"alpha NEW tail"),
            (b"middle.bin", &[b'x'; 128]),
            (b"z.txt", b"omega NEW tail"),
        ]);
        let edits = derive_edits(&base, &child);
        assert_eq!(edits.len(), 2, "two distant modified files should yield two edits");
        assert_eq!(apply_edits(&base, &edits).unwrap(), child);
        let inserted: usize = edits.iter().map(|edit| edit.insert.len()).sum();
        assert!(inserted < 64, "unchanged middle file must not be rewritten");
    }

    #[test]
    fn pack_diff_handles_added_and_deleted_files() {
        let base = pack(&[(b"a.txt", b"a"), (b"gone.txt", b"old"), (b"z.txt", b"z")]);
        let child = pack(&[(b"a.txt", b"a"), (b"new.txt", b"new"), (b"z.txt", b"z")]);
        let edits = derive_edits(&base, &child);
        assert_eq!(apply_edits(&base, &edits).unwrap(), child);
        assert!(edits.len() >= 1);
    }
}
