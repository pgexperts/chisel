//! O(1)-operation LRU index over `u64` page IDs.
//!
//! Replaces the original `VecDeque<u64> + retain` LRU implementation in
//! `page_cache.rs`. The deque-based version was documented as a v1
//! trade-off:
//!
//! > "O(n) in the cache size due to the `retain` scan. Acceptable for v1
//! > because `max_pages` is small (hundreds to low thousands); a proper
//! > intrusive linked list would be the optimization if this shows up in
//! > profiles." — `page_cache.rs:548` (pre-this-change comment)
//!
//! It showed up in profiles. A 70k-row INSERT spent 66% of CPU in
//! `_platform_memmove`, almost entirely from `VecDeque::retain` shifting
//! elements as pages were touched.
//!
//! ## Implementation
//!
//! A doubly-linked list is maintained over IDs, with neighbour pointers
//! stored in a single `FxHashMap<u64, Node>` (I77: a fast hasher over the
//! trusted u64 page-id keys) rather than as `Box`/`Rc`
//! pointers in heap-allocated nodes. Each `Node` holds `(prev, next)`
//! IDs; `head` is the most-recently-used (MRU) end, `tail` is the
//! least-recently-used (LRU) end.
//!
//! Operations:
//!
//! | op                     | cost  | use site                              |
//! |------------------------|-------|---------------------------------------|
//! | [`push_front`]         | O(1)  | mark a page as MRU (insert or move)   |
//! | [`remove`]             | O(1)  | drop a page from the LRU              |
//! | [`contains`]           | O(1)  | LRU presence check                    |
//! | [`iter_lru_to_mru`]    | O(1)  | start an eviction scan from LRU end   |
//! | iterator step          | O(1)  | one step toward MRU                   |
//!
//! All amortised constant — no shifts, no scans, no realloc spikes.
//!
//! ## Correctness model
//!
//! Two map entries hold redundant information about each linked
//! pair: A's `next` points to B, and B's `prev` points to A. Mutators
//! must update both sides of every neighbour edge. The `unlink` helper
//! is the single place that does this; all public mutators call through
//! it (or through the inserter, which has its own paired-update logic).
//! That keeps the redundancy exposure to one routine.
//!
//! `head` and `tail` are derived state — they always equal the IDs of
//! the prev=None and next=None nodes respectively. They're cached so
//! `iter_lru_to_mru` and `push_front` don't have to scan to find them.
//!
//! No `unsafe`. Fits in cache-line-sized state per node (16 bytes for
//! the `Node` plus map overhead).

use rustc_hash::FxHashMap;

#[derive(Debug, Clone, Copy)]
struct Node {
    prev: Option<u64>,
    next: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct LruIndex {
    /// MRU end. `Some` iff the index is non-empty.
    head: Option<u64>,
    /// LRU end. `Some` iff the index is non-empty.
    tail: Option<u64>,
    /// Per-id neighbour pointers. The set of keys IS the membership.
    /// I77: FxHashMap, not default SipHash — keys are trusted u64 page ids
    /// (no DoS surface) and every `push_front`/`unlink` probes this map.
    nodes: FxHashMap<u64, Node>,
}

impl LruIndex {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // used by unit tests; kept on the API for parity with VecDeque
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[allow(dead_code)] // used by unit tests; kept on the API for parity with VecDeque
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn contains(&self, id: u64) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Set `id` as the most-recently-used.
    ///
    /// If `id` is already present, it moves to the MRU end. If absent,
    /// it's inserted at the MRU end. O(1) either way.
    pub fn push_front(&mut self, id: u64) {
        if self.contains(id) {
            self.unlink(id);
        }
        // unlink-first is load-bearing: the old-head `get_mut` below borrows
        // `nodes` and would alias `id`'s own entry if it were still linked in.
        // After unlink, `id`'s entry (if any) is detached, so patching the
        // neighbour and the final `insert` touch disjoint slots. The `insert`
        // must come last — patching `old_head.prev` first keeps the back-edge
        // consistent before `id` exists as a node.
        // Re-link at head.
        let new_node = Node {
            prev: None,
            next: self.head,
        };
        match self.head {
            Some(old_head) => {
                if let Some(n) = self.nodes.get_mut(&old_head) {
                    n.prev = Some(id);
                }
            }
            None => {
                // List was empty; this id is also the new tail.
                self.tail = Some(id);
            }
        }
        self.nodes.insert(id, new_node);
        self.head = Some(id);
    }

    /// Insert (or move) `id` to the LRU-tail end (least recently used).
    ///
    /// The drain-insertion policy uses this when `DrainInsertion::LruTail`
    /// is set: rehydrated-but-not-yet-needed pages become first eviction
    /// candidates so hot in-flight pages stay in the MRU region.
    ///
    /// If `id` is already present it moves to the tail. O(1) either way —
    /// mirrors `push_front` but inserts adjacent to `tail` instead of `head`.
    pub fn push_back(&mut self, id: u64) {
        if self.contains(id) {
            self.unlink(id);
        }
        // Re-link at tail (LRU end). tail.next is None by invariant;
        // the new node's next is also None, making it the new tail.
        let new_node = Node {
            prev: self.tail,
            next: None,
        };
        match self.tail {
            Some(old_tail) => {
                if let Some(n) = self.nodes.get_mut(&old_tail) {
                    n.next = Some(id);
                }
            }
            None => {
                // List was empty; this id is also the new head.
                self.head = Some(id);
            }
        }
        self.nodes.insert(id, new_node);
        self.tail = Some(id);
    }

    /// Remove `id` from the LRU. Returns `true` if it was present.
    pub fn remove(&mut self, id: u64) -> bool {
        if !self.contains(id) {
            return false;
        }
        self.unlink(id);
        self.nodes.remove(&id);
        true
    }

    /// Iterate from LRU (oldest) toward MRU (newest). The first item
    /// is the eviction candidate; the last is the most-recently-used.
    pub fn iter_lru_to_mru(&self) -> LruIter<'_> {
        LruIter {
            idx: self,
            cursor: self.tail,
        }
    }

    /// Unlink `id` from the doubly-linked list (patch up its neighbours
    /// and the head/tail pointers as needed) but do NOT remove it from
    /// `nodes`. Callers either remove afterward or re-link.
    ///
    /// Safe to call when `id` is not present (no-op).
    fn unlink(&mut self, id: u64) {
        let node = match self.nodes.get(&id) {
            Some(n) => *n,
            None => return,
        };
        match node.prev {
            Some(prev_id) => {
                if let Some(p) = self.nodes.get_mut(&prev_id) {
                    p.next = node.next;
                }
            }
            None => {
                // id was head; advance head past it.
                self.head = node.next;
            }
        }
        match node.next {
            Some(next_id) => {
                if let Some(n) = self.nodes.get_mut(&next_id) {
                    n.prev = node.prev;
                }
            }
            None => {
                // id was tail; back tail up.
                self.tail = node.prev;
            }
        }
    }
}

pub(crate) struct LruIter<'a> {
    idx: &'a LruIndex,
    cursor: Option<u64>,
}

impl Iterator for LruIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        let cur = self.cursor?;
        // Step toward the MRU end (toward head). prev points there
        // because the list is "head=MRU, tail=LRU" and we walk
        // tail-to-head via `prev` links.
        self.cursor = self.idx.nodes.get(&cur).and_then(|n| n.prev);
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_lru_to_mru(idx: &LruIndex) -> Vec<u64> {
        idx.iter_lru_to_mru().collect()
    }

    #[test]
    fn empty() {
        let idx = LruIndex::new();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert!(!idx.contains(1));
        assert_eq!(collect_lru_to_mru(&idx), Vec::<u64>::new());
    }

    #[test]
    fn push_one() {
        let mut idx = LruIndex::new();
        idx.push_front(7);
        assert_eq!(idx.len(), 1);
        assert!(idx.contains(7));
        assert_eq!(collect_lru_to_mru(&idx), vec![7]);
    }

    #[test]
    fn push_three_then_iterate_lru_to_mru() {
        let mut idx = LruIndex::new();
        // Insertion order: 1, 2, 3 → head=3 (MRU), tail=1 (LRU).
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3);
        assert_eq!(collect_lru_to_mru(&idx), vec![1, 2, 3]);
    }

    #[test]
    fn push_existing_moves_to_front() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3);
        // Touch 1 → it moves to MRU end.
        idx.push_front(1);
        // LRU-to-MRU now: 2, 3, 1.
        assert_eq!(collect_lru_to_mru(&idx), vec![2, 3, 1]);
        // Length unchanged (still 3, no duplicates).
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn remove_middle() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3);
        // List: tail=1, mid=2, head=3.
        assert!(idx.remove(2));
        assert_eq!(collect_lru_to_mru(&idx), vec![1, 3]);
        assert_eq!(idx.len(), 2);
        assert!(!idx.contains(2));
    }

    #[test]
    fn remove_head() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3);
        assert!(idx.remove(3));
        assert_eq!(collect_lru_to_mru(&idx), vec![1, 2]);
    }

    #[test]
    fn remove_tail() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3);
        assert!(idx.remove(1));
        assert_eq!(collect_lru_to_mru(&idx), vec![2, 3]);
    }

    #[test]
    fn remove_only() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        assert!(idx.remove(1));
        assert!(idx.is_empty());
        assert_eq!(collect_lru_to_mru(&idx), Vec::<u64>::new());
        // After empty, push works.
        idx.push_front(2);
        assert_eq!(collect_lru_to_mru(&idx), vec![2]);
    }

    #[test]
    fn remove_absent_is_noop() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        assert!(!idx.remove(99));
        assert_eq!(collect_lru_to_mru(&idx), vec![1]);
    }

    #[test]
    fn push_back_inserts_at_tail() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2); // 2 is MRU, 1 is LRU: [1, 2] (LRU→MRU)
        idx.push_back(3); // 3 should become new LRU
        let order: Vec<u64> = collect_lru_to_mru(&idx);
        assert_eq!(order, vec![3, 1, 2]);
    }

    #[test]
    fn push_back_moves_existing_to_tail() {
        let mut idx = LruIndex::new();
        idx.push_front(1);
        idx.push_front(2);
        idx.push_front(3); // 3=MRU, 2=mid, 1=LRU
                           // Move 2 to LRU tail.
        idx.push_back(2);
        // LRU-to-MRU now: 2, 1, 3.
        assert_eq!(collect_lru_to_mru(&idx), vec![2, 1, 3]);
        assert_eq!(idx.len(), 3);
    }

    #[test]
    fn push_back_on_empty() {
        let mut idx = LruIndex::new();
        idx.push_back(5);
        assert_eq!(collect_lru_to_mru(&idx), vec![5]);
        assert_eq!(idx.head, Some(5));
        assert_eq!(idx.tail, Some(5));
    }

    #[test]
    fn churn_keeps_invariants() {
        // Fuzz-ish: lots of pushes and removes, verify membership consistency.
        let mut idx = LruIndex::new();
        // Add 100, then remove every 3rd, then re-touch every 5th.
        for i in 0..100 {
            idx.push_front(i);
        }
        assert_eq!(idx.len(), 100);
        for i in 0..100 {
            if i % 3 == 0 {
                idx.remove(i);
            }
        }
        for i in 0..100 {
            if i % 5 == 0 && i % 3 != 0 {
                idx.push_front(i);
            }
        }
        // Just check that the list is internally consistent: the set of
        // ids visited by iter_lru_to_mru equals the set returned by
        // contains-checking every id.
        let visited: Vec<u64> = collect_lru_to_mru(&idx);
        assert_eq!(visited.len(), idx.len());
        for id in &visited {
            assert!(idx.contains(*id));
        }
        // Every id 0..100 either is in the index or isn't, never both.
        let mut count_present = 0;
        for i in 0..100 {
            if idx.contains(i) {
                count_present += 1;
            }
        }
        assert_eq!(count_present, idx.len());
    }
}
