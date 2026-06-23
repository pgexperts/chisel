//! transaction::read — read-side API: read / tag / client_byte /
//! handles / handles_with_tag (+ their `_inner` cores) and the live-handle
//! lookup helpers. Split out of `transaction.rs` verbatim; see the parent
//! module for the type and fields.

use super::*;

impl TransactionManager {
    pub fn tag(&self, handle: u64) -> Result<u32> {
        self.check_alive()?;
        let result = self.tag_inner(handle);
        self.poison_on_fatal(result)
    }

    /// The handle-table root for the *current read view*: the in-progress
    /// `current_roots` while a transaction is active (read-your-own-writes),
    /// otherwise the last durably-committed `committed_roots`. Returns
    /// `PAGE_ID_NONE` for an empty database — read paths guard on that
    /// before walking the tree. Centralizes the snapshot selection shared
    /// by every read-path helper (`tag`, `client_byte`, `read`, `handles`,
    /// `handle_live_page_id`).
    pub(super) fn live_handle_table_root(&self) -> u64 {
        if self.active_txn {
            self.current_roots.handle_table_page
        } else {
            self.committed_roots.handle_table_page
        }
    }

    /// Look up a handle that must be live, applying the "deleted ⇒
    /// `InvalidHandle`" rule in ONE place (I125). Every read/mutation entry
    /// point that needs a live `HandleEntry` — `read`, `tag`, `client_byte`,
    /// `set_client_byte`, `update`, `delete_tagged` — goes through here, so the
    /// liveness invariant cannot drift between callers.
    ///
    /// `handle_table::lookup` already collapses a tombstone (and an empty/absent
    /// tree, via `live_handle_table_root` returning `PAGE_ID_NONE`) to `None`,
    /// so the `ok_or` below is the single site that raises the operational
    /// `InvalidHandle`. Callers that want "absent is not an error" (e.g.
    /// `handle_live_page_id`, which returns `Ok(None)`) deliberately do NOT use
    /// this and keep their own Option-returning lookup.
    pub(super) fn lookup_live(&self, handle: u64) -> Result<HandleEntry> {
        let root = self.live_handle_table_root();
        let mut cache = self.cache.borrow_mut();
        self.handle_table
            .lookup(&mut cache, root, handle)?
            .ok_or(ChiselError::InvalidHandle(handle))
    }

    fn tag_inner(&self, handle: u64) -> Result<u32> {
        Ok(self.lookup_live(handle)?.tag)
    }

    /// Return the opaque client byte stored in `handle`'s entry. Returns 0 if
    /// never set (including every chunk created before this feature). Rejects
    /// deleted handles with `InvalidHandle` via the shared `lookup_live` guard
    /// (I125 — `read`, `tag`, and `delete_tagged` apply the identical rule).
    /// Takes `&self`.
    pub fn client_byte(&self, handle: u64) -> Result<u8> {
        self.check_alive()?;
        let result = self.client_byte_inner(handle);
        self.poison_on_fatal(result)
    }

    fn client_byte_inner(&self, handle: u64) -> Result<u8> {
        Ok(self.lookup_live(handle)?.client_byte)
    }

    /// Set the opaque client byte for `handle`. Requires an active
    /// transaction; durable on commit, reverted on rollback. Any `u8` is
    /// valid. COWs only the handle-table leaf — no data-page, overflow, or
    /// membership-index work. Takes `&mut self`.
    pub fn set_client_byte(&mut self, handle: u64, byte: u8) -> Result<()> {
        self.check_alive()?;
        let result = self.set_client_byte_inner(handle, byte);
        self.poison_on_fatal(result)
    }

    fn set_client_byte_inner(&mut self, handle: u64, byte: u8) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let mut entry = self.lookup_live(handle)?;
        entry.client_byte = byte;
        self.ht_insert(handle, &entry)?;
        Ok(())
    }

    pub fn handles_with_tag(&self, tag: u32) -> Result<Vec<u64>> {
        self.check_alive()?;
        let result = self.handles_with_tag_inner(tag);
        self.poison_on_fatal(result)
    }

    fn handles_with_tag_inner(&self, tag: u32) -> Result<Vec<u64>> {
        let root = if self.active_txn {
            self.current_roots.membership_index_page
        } else {
            self.committed_roots.membership_index_page
        };
        let mut cache = self.cache.borrow_mut();
        // No PAGE_ID_NONE guard (unlike tag_inner): an empty/absent index is a
        // legitimate "no handles with this tag" -> handles_for_tag returns an
        // empty Vec for a PAGE_ID_NONE root, whereas a missing handle table is an error.
        self.membership_index.handles_for_tag(&mut cache, root, tag)
    }

    /// Read a value by handle.
    ///
    /// If a transaction is active, reads see the in-progress (uncommitted) state
    /// through current_roots — i.e. "read your own writes". Otherwise reads go
    /// through committed_roots, the last durably-committed snapshot. There is no
    /// MVCC / snapshot isolation for concurrent readers because the writer is
    /// single-threaded; this branch is purely about making the active writer
    /// see its own pending mutations.
    ///
    /// F3: takes `&self`. Internally, the page cache is wrapped in a
    /// RefCell so that the mutation required by LRU bookkeeping / page
    /// loading can happen behind a shared reference. See the field-level
    /// comment on `cache` for the full rationale and why RefCell was
    /// chosen over Mutex.
    pub fn read(&self, handle: u64) -> Result<Vec<u8>> {
        self.check_alive()?;
        let result = self.read_inner(handle);
        self.poison_on_fatal(result)
    }

    fn read_inner(&self, handle: u64) -> Result<Vec<u8>> {
        let entry = self.lookup_live(handle)?;

        let mut cache = self.cache.borrow_mut();
        match entry.flags {
            HandleFlags::Live => {
                let buf = cache.get(entry.page_id)?;
                // The handle-table entry insists this slot is live. If
                // `DataPage::read` returns None anyway, the data page's
                // structural state disagrees with the handle table — the
                // page header, slot directory, or slot entry is damaged.
                // That's CorruptPage (fatal / poisons the manager), not
                // InvalidHandle (operational).
                match DataPage::read(buf, entry.slot_index) {
                    Some(data) => Ok(data.to_vec()),
                    None => Err(ChiselError::CorruptPage {
                        page_id: entry.page_id,
                    }),
                }
            }
            HandleFlags::Overflow => Overflow::read(&mut cache, entry.page_id),
            // Unreachable in practice: `lookup_live` already excludes tombstones
            // (I125). Kept as an exhaustive, non-panicking backstop — if the
            // liveness invariant were ever violated, read still returns the
            // operational `InvalidHandle` rather than aborting the writer.
            HandleFlags::Deleted => Err(ChiselError::InvalidHandle(handle)),
        }
    }

    /// Iterate over all live handles.
    ///
    /// F3: takes `&self` (same rationale as `read`).
    pub fn handles(&self) -> Result<Vec<u64>> {
        self.check_alive()?;
        let result = self.handles_inner();
        self.poison_on_fatal(result)
    }

    fn handles_inner(&self) -> Result<Vec<u64>> {
        let root = self.live_handle_table_root();
        if root == PAGE_ID_NONE {
            return Ok(Vec::new());
        }
        let mut cache = self.cache.borrow_mut();
        let entries = self.handle_table.iter_live(&mut cache, root)?;
        Ok(entries.into_iter().map(|(h, _)| h).collect())
    }
}
