use crate::model::{Backend, BackendStats, Edit, StateError};
use std::cmp::max;
use std::mem::size_of;
use std::sync::{Arc, Weak};

#[derive(Clone)]
pub struct Snapshot(Option<Arc<Node>>);

#[derive(Clone)]
struct Piece {
    buffer: Arc<[u8]>,
    start: usize,
    len: usize,
}

struct Node {
    len: usize,
    height: usize,
    kind: Kind,
}

enum Kind {
    Leaf(Piece),
    Branch { left: Arc<Node>, right: Arc<Node> },
}

struct TrackedNode {
    weak: Weak<Node>,
}

struct TrackedBuffer {
    weak: Weak<[u8]>,
    bytes: usize,
}

/// Persistent copy-on-write piece rope.
///
/// This intentionally uses the same broad path-copying balanced-tree idea as
/// the AVL byte rope, but leaf pieces point into immutable shared buffers.
/// Splitting a piece therefore allocates metadata only; it does not copy the
/// bytes on either side of an edit. Each inserted payload is allocated once.
pub struct PieceCow {
    leaf_bytes: usize,
    nodes: Vec<TrackedNode>,
    buffers: Vec<TrackedBuffer>,
    lifetime_payload_bytes: usize,
    lifetime_metadata_bytes: usize,
}

impl PieceCow {
    pub fn new(leaf_bytes: usize) -> Self {
        assert!(leaf_bytes > 0, "leaf_bytes must be positive");
        Self {
            leaf_bytes,
            nodes: Vec::new(),
            buffers: Vec::new(),
            lifetime_payload_bytes: 0,
            lifetime_metadata_bytes: 0,
        }
    }

    fn alloc_buffer(&mut self, bytes: &[u8]) -> Option<Arc<[u8]>> {
        if bytes.is_empty() {
            return None;
        }
        let buffer: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
        self.lifetime_payload_bytes = self
            .lifetime_payload_bytes
            .saturating_add(buffer.len());
        self.buffers.push(TrackedBuffer {
            weak: Arc::downgrade(&buffer),
            bytes: buffer.len(),
        });
        Some(buffer)
    }

    fn alloc_leaf(&mut self, buffer: Arc<[u8]>, start: usize, len: usize) -> Arc<Node> {
        debug_assert!(len > 0);
        debug_assert!(start.saturating_add(len) <= buffer.len());
        let node = Arc::new(Node {
            len,
            height: 1,
            kind: Kind::Leaf(Piece { buffer, start, len }),
        });
        self.lifetime_metadata_bytes = self
            .lifetime_metadata_bytes
            .saturating_add(size_of::<Node>());
        self.nodes.push(TrackedNode {
            weak: Arc::downgrade(&node),
        });
        node
    }

    fn alloc_branch(&mut self, left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
        let node = Arc::new(Node {
            len: left.len.saturating_add(right.len),
            height: max(left.height, right.height).saturating_add(1),
            kind: Kind::Branch { left, right },
        });
        self.lifetime_metadata_bytes = self
            .lifetime_metadata_bytes
            .saturating_add(size_of::<Node>());
        self.nodes.push(TrackedNode {
            weak: Arc::downgrade(&node),
        });
        node
    }

    fn build_from_buffer(
        &mut self,
        buffer: Arc<[u8]>,
        start: usize,
        len: usize,
    ) -> Option<Arc<Node>> {
        if len == 0 {
            return None;
        }
        let mut leaves = Vec::with_capacity(len.div_ceil(self.leaf_bytes));
        let mut offset = 0usize;
        while offset < len {
            let take = (len - offset).min(self.leaf_bytes);
            leaves.push(self.alloc_leaf(buffer.clone(), start + offset, take));
            offset += take;
        }
        self.build_from_leaves(&leaves)
    }

    fn build_bytes(&mut self, bytes: &[u8]) -> Option<Arc<Node>> {
        let buffer = self.alloc_buffer(bytes)?;
        self.build_from_buffer(buffer, 0, bytes.len())
    }

    fn build_from_leaves(&mut self, leaves: &[Arc<Node>]) -> Option<Arc<Node>> {
        match leaves.len() {
            0 => None,
            1 => Some(leaves[0].clone()),
            n => {
                let mid = n / 2;
                let left = self.build_from_leaves(&leaves[..mid]).unwrap();
                let right = self.build_from_leaves(&leaves[mid..]).unwrap();
                Some(self.alloc_branch(left, right))
            }
        }
    }

    fn join(
        &mut self,
        left: Option<Arc<Node>>,
        right: Option<Arc<Node>>,
    ) -> Option<Arc<Node>> {
        match (left, right) {
            (None, r) => r,
            (l, None) => l,
            (Some(l), Some(r)) => Some(self.join_nodes(l, r)),
        }
    }

    fn join_nodes(&mut self, left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
        if left.height > right.height.saturating_add(1) {
            match &left.kind {
                Kind::Branch {
                    left: left_left,
                    right: left_right,
                } => {
                    let joined = self.join_nodes(left_right.clone(), right);
                    self.balance(left_left.clone(), joined)
                }
                Kind::Leaf(_) => unreachable!("a leaf cannot be two levels taller"),
            }
        } else if right.height > left.height.saturating_add(1) {
            match &right.kind {
                Kind::Branch {
                    left: right_left,
                    right: right_right,
                } => {
                    let joined = self.join_nodes(left, right_left.clone());
                    self.balance(joined, right_right.clone())
                }
                Kind::Leaf(_) => unreachable!("a leaf cannot be two levels taller"),
            }
        } else {
            self.alloc_branch(left, right)
        }
    }

    fn balance(&mut self, left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
        if left.height > right.height.saturating_add(1) {
            match &left.kind {
                Kind::Branch {
                    left: ll,
                    right: lr,
                } => {
                    if ll.height >= lr.height {
                        let new_right = self.alloc_branch(lr.clone(), right);
                        self.alloc_branch(ll.clone(), new_right)
                    } else {
                        match &lr.kind {
                            Kind::Branch {
                                left: lrl,
                                right: lrr,
                            } => {
                                let new_left = self.alloc_branch(ll.clone(), lrl.clone());
                                let new_right = self.alloc_branch(lrr.clone(), right);
                                self.alloc_branch(new_left, new_right)
                            }
                            Kind::Leaf(_) => unreachable!("double rotation requires branch"),
                        }
                    }
                }
                Kind::Leaf(_) => unreachable!("imbalanced left side must be a branch"),
            }
        } else if right.height > left.height.saturating_add(1) {
            match &right.kind {
                Kind::Branch {
                    left: rl,
                    right: rr,
                } => {
                    if rr.height >= rl.height {
                        let new_left = self.alloc_branch(left, rl.clone());
                        self.alloc_branch(new_left, rr.clone())
                    } else {
                        match &rl.kind {
                            Kind::Branch {
                                left: rll,
                                right: rlr,
                            } => {
                                let new_left = self.alloc_branch(left, rll.clone());
                                let new_right = self.alloc_branch(rlr.clone(), rr.clone());
                                self.alloc_branch(new_left, new_right)
                            }
                            Kind::Leaf(_) => unreachable!("double rotation requires branch"),
                        }
                    }
                }
                Kind::Leaf(_) => unreachable!("imbalanced right side must be a branch"),
            }
        } else {
            self.alloc_branch(left, right)
        }
    }

    fn split_node(
        &mut self,
        root: Arc<Node>,
        pos: usize,
    ) -> (Option<Arc<Node>>, Option<Arc<Node>>) {
        debug_assert!(pos <= root.len);
        if pos == 0 {
            return (None, Some(root));
        }
        if pos == root.len {
            return (Some(root), None);
        }

        match &root.kind {
            Kind::Leaf(piece) => {
                let left = self.alloc_leaf(piece.buffer.clone(), piece.start, pos);
                let right = self.alloc_leaf(
                    piece.buffer.clone(),
                    piece.start + pos,
                    piece.len - pos,
                );
                (Some(left), Some(right))
            }
            Kind::Branch { left, right } => {
                if pos < left.len {
                    let (a, b) = self.split_node(left.clone(), pos);
                    let new_right = self.join(b, Some(right.clone()));
                    (a, new_right)
                } else if pos == left.len {
                    (Some(left.clone()), Some(right.clone()))
                } else {
                    let (a, b) = self.split_node(right.clone(), pos - left.len);
                    let new_left = self.join(Some(left.clone()), a);
                    (new_left, b)
                }
            }
        }
    }

    fn append_range(node: &Node, start: usize, len: usize, out: &mut Vec<u8>) {
        if len == 0 {
            return;
        }
        match &node.kind {
            Kind::Leaf(piece) => {
                let begin = piece.start + start;
                out.extend_from_slice(&piece.buffer[begin..begin + len]);
            }
            Kind::Branch { left, right } => {
                if start < left.len {
                    let left_len = (left.len - start).min(len);
                    Self::append_range(left, start, left_len, out);
                    let remaining = len - left_len;
                    if remaining > 0 {
                        Self::append_range(right, 0, remaining, out);
                    }
                } else {
                    Self::append_range(right, start - left.len, len, out);
                }
            }
        }
    }

    fn validate_node(&self, node: &Node) -> Result<(usize, usize), String> {
        match &node.kind {
            Kind::Leaf(piece) => {
                if piece.len == 0 {
                    return Err("empty piece".into());
                }
                if piece.len > self.leaf_bytes {
                    return Err("piece exceeds configured leaf size".into());
                }
                let end = piece
                    .start
                    .checked_add(piece.len)
                    .ok_or_else(|| "piece range overflow".to_string())?;
                if end > piece.buffer.len() {
                    return Err("piece exceeds backing buffer".into());
                }
                if node.len != piece.len || node.height != 1 {
                    return Err("piece leaf cache mismatch".into());
                }
                Ok((piece.len, 1))
            }
            Kind::Branch { left, right } => {
                let (ll, lh) = self.validate_node(left)?;
                let (rl, rh) = self.validate_node(right)?;
                let expected_len = ll
                    .checked_add(rl)
                    .ok_or_else(|| "length overflow".to_string())?;
                let expected_height = max(lh, rh) + 1;
                if node.len != expected_len || node.height != expected_height {
                    return Err("branch cache mismatch".into());
                }
                if lh > rh + 1 || rh > lh + 1 {
                    return Err(format!("AVL imbalance: left_height={lh}, right_height={rh}"));
                }
                Ok((expected_len, expected_height))
            }
        }
    }
}

impl Backend for PieceCow {
    type Snapshot = Snapshot;

    fn name(&self) -> &'static str {
        "persistent-piece-cow"
    }

    fn create(&mut self, bytes: &[u8]) -> Self::Snapshot {
        Snapshot(self.build_bytes(bytes))
    }

    fn len(&self, snapshot: &Self::Snapshot) -> usize {
        snapshot.0.as_ref().map_or(0, |root| root.len)
    }

    fn read_range(
        &self,
        snapshot: &Self::Snapshot,
        start: usize,
        len: usize,
    ) -> Result<Vec<u8>, StateError> {
        let total = self.len(snapshot);
        let end = start.checked_add(len).ok_or(StateError::LengthOverflow)?;
        if start > total || end > total {
            return Err(StateError::OutOfBounds {
                len: total,
                start,
                delete_len: len,
            });
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let root = snapshot
            .0
            .as_ref()
            .ok_or(StateError::Corrupt("non-empty read from empty piece rope"))?;
        let mut out = Vec::with_capacity(len);
        Self::append_range(root, start, len, &mut out);
        Ok(out)
    }

    fn edit(
        &mut self,
        parent: &Self::Snapshot,
        edit: &Edit,
    ) -> Result<Self::Snapshot, StateError> {
        let parent_len = self.len(parent);
        edit.validate_len(parent_len)?;
        let expected_len = edit.output_len(parent_len)?;

        let (left, after_left) = match parent.0.clone() {
            None => (None, None),
            Some(root) => self.split_node(root, edit.start),
        };
        let (_deleted, right) = match after_left {
            None => (None, None),
            Some(rest) => self.split_node(rest, edit.delete_len),
        };
        let inserted = self.build_bytes(&edit.insert);
        let with_insert = self.join(left, inserted);
        let root = self.join(with_insert, right);
        let snapshot = Snapshot(root);
        if self.len(&snapshot) != expected_len {
            return Err(StateError::Corrupt("piece edit produced wrong length"));
        }
        Ok(snapshot)
    }

    fn validate(&self, snapshot: &Self::Snapshot) -> Result<(), String> {
        if let Some(root) = &snapshot.0 {
            self.validate_node(root)?;
        }
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        let mut retained_payload_bytes = 0usize;
        let mut live_buffers = 0usize;
        for tracked in &self.buffers {
            if tracked.weak.upgrade().is_some() {
                live_buffers += 1;
                retained_payload_bytes = retained_payload_bytes.saturating_add(tracked.bytes);
            }
        }

        let mut live_nodes = 0usize;
        for tracked in &self.nodes {
            if tracked.weak.upgrade().is_some() {
                live_nodes += 1;
            }
        }
        let retained_metadata_bytes = live_nodes.saturating_mul(size_of::<Node>());

        BackendStats {
            retained_payload_bytes,
            retained_metadata_bytes,
            lifetime_payload_bytes: self.lifetime_payload_bytes,
            lifetime_metadata_bytes: self.lifetime_metadata_bytes,
            live_objects: live_buffers.saturating_add(live_nodes),
            total_objects_allocated: self.buffers.len().saturating_add(self.nodes.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_is_exact_and_parent_is_unchanged() {
        let mut pieces = PieceCow::new(4);
        let base = pieces.create(b"abcdefghij");
        let child = pieces
            .edit(
                &base,
                &Edit {
                    start: 3,
                    delete_len: 4,
                    insert: b"XYZ".to_vec(),
                },
            )
            .unwrap();

        assert_eq!(pieces.read_all(&base).unwrap(), b"abcdefghij");
        assert_eq!(pieces.read_all(&child).unwrap(), b"abcXYZhij");
        pieces.validate(&base).unwrap();
        pieces.validate(&child).unwrap();
    }

    #[test]
    fn split_reuses_payload_buffer() {
        let mut pieces = PieceCow::new(4096);
        let bytes = vec![b'x'; 4096];
        let base = pieces.create(&bytes);
        let before = pieces.stats().retained_payload_bytes;
        let child = pieces
            .edit(
                &base,
                &Edit {
                    start: 2048,
                    delete_len: 1,
                    insert: Vec::new(),
                },
            )
            .unwrap();
        let after = pieces.stats().retained_payload_bytes;
        assert_eq!(before, after, "splitting/deleting should not copy old payload");
        assert_eq!(pieces.read_all(&child).unwrap().len(), 4095);
    }

    #[test]
    fn repeated_historical_branching_stays_exact() {
        let mut pieces = PieceCow::new(32);
        let base_bytes = (0..2048).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let base = pieces.create(&base_bytes);
        let mut snapshots = vec![base];
        let mut oracle = vec![base_bytes];
        let mut x = 0x9e37_79b9_7f4a_7c15u64;

        for i in 0..400usize {
            x ^= x << 7;
            x ^= x >> 9;
            let parent = (x as usize) % snapshots.len();
            let parent_bytes = &oracle[parent];
            x ^= x << 8;
            let start = (x as usize) % (parent_bytes.len() + 1);
            let available = parent_bytes.len() - start;
            let delete_len = available.min(i % 11);
            let insert = vec![b'a' + (i % 26) as u8; i % 13];
            let edit = Edit {
                start,
                delete_len,
                insert,
            };
            let expected = edit.apply(parent_bytes).unwrap();
            let child = pieces.edit(&snapshots[parent], &edit).unwrap();
            pieces.validate(&child).unwrap();
            assert_eq!(pieces.read_all(&child).unwrap(), expected);
            snapshots.push(child);
            oracle.push(expected);
        }
    }
}
