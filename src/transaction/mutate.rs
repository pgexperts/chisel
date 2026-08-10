//! transaction::mutate — mutation API: update / delete / delete_tagged /
//! delete_with_tag / delete_many (+ their `_inner` cores). Split out of
//! `transaction.rs` verbatim; see the parent module for the type and fields.

use super::*;

impl TransactionManager {
    /// Update an existing handle to point at a new value.
    ///
    /// Allocates a new slot/overflow chain for the new value and rewrites
    /// the HandleEntry via COW. The OLD location is retired differently
    /// depending on its kind:
    ///
    ///   * Inline (Live): goes through `release_data_slot`, which
    ///     decrements the per-page live-slot count (R1). Only when the
    ///     count reaches zero does the entire page land in
    ///     `txn_freed_pages`. Otherwise the slot becomes a tombstone,
    ///     reclaimable only via defrag (R3).
    ///   * Overflow: the whole chain is deleted and every page in the
    ///     chain is pushed onto `txn_freed_pages`.
    ///
    /// The earlier "assumes one live slot per page / must change when R1
    /// lands" caveat is OBSOLETE — R1 has landed and the slot-level
    /// accounting below implements exactly the post-R1 contract.
    pub fn update(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        self.check_alive()?;
        let result = self.update_inner(handle, value);
        self.poison_on_fatal(result)
    }

    fn update_inner(&mut self, handle: u64, value: &[u8]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // `update` requires an active txn (checked above), so `lookup_live`'s
        // read-view root is `current_roots` — read-your-own-writes.
        let entry = self.lookup_live(handle)?;

        // Test-only injection (see `force_deleted_old_entry`). The Deleted arm of
        // the OldRelease match below is unreachable by construction — `lookup_live`
        // never yields a tombstone — so this shadow binding is the only way to
        // cover its I45 escalation. Shadowing rather than mutating keeps the
        // production binding immutable and leaves the non-test build byte-identical
        // — the whole block is absent, so `entry` is still `lookup_live`'s result.
        // Same discipline as `delete_inner`'s `let res` injection.
        #[cfg(test)]
        let entry = if self.fault.force_deleted_old_entry.replace(false) {
            HandleEntry {
                flags: HandleFlags::Deleted,
                ..entry
            }
        } else {
            entry
        };

        // Atomic staging (same discipline as delete_inner): do NOT retire the
        // OLD value's storage until the NEW entry is durably installed. The
        // previous "free old first" ordering meant a non-fatal CacheFull during
        // the new-value write or the handle-table install left the committed
        // handle still pointing at pages already queued for reclamation — a
        // reachable-but-free page that commit then frees, corrupting the live
        // value on the next freemap reuse. We compute the new value, the new
        // handle-table root, and the old-location free set in a fallible PREPARE
        // phase that touches no installed state, then install the new entry and
        // retire the old location together in an infallible INSTALL phase. A
        // mid-prepare failure is a complete no-op.
        //
        // Depth: `HandleTable::insert` unwinds its own eager grow-bump when IT
        // fails (HANDLES-INDEX-3, issue #107), so the candidate-computation
        // failure below is covered. The overflow-walk failure further down is
        // the other case — `insert` returned Ok, so a grow may really have
        // happened, and we then discard the candidate root. `update` replaces
        // an EXISTING handle, which `lookup_live` has already resolved, so
        // `handle < capacity()` and no grow can occur; but that is an
        // inter-module argument, and the same shape of prose argument is what
        // let this bug exist in the first place. The save/restore below makes it
        // structural for two lines, matching `abort_allocate_prepare`.

        // Test-only injection (see `fail_next_update_value_write`): now the FIRST
        // fallible step, so a simulated failure here retires nothing.
        #[cfg(test)]
        if self.fault.fail_next_update_value_write.replace(false) {
            return Err(ChiselError::CacheFull { limit: 0 });
        }

        // PREPARE: write the new value storage. Tags and the client byte are
        // entry-resident and carried forward unchanged (the handle is unchanged,
        // so the membership index needs no edit — only the value's storage
        // moves). Capture the inline data page so a later prepare failure can
        // release it, keeping live-slot / cursor bookkeeping consistent.
        let mut new_inline_page: Option<u64> = None;
        let new_entry = if value.len() > MAX_INLINE_VALUE {
            let first_page = {
                let mut cache = self.cache.borrow_mut();
                Overflow::write(&mut cache, value)?
            };
            HandleEntry {
                page_id: first_page,
                slot_index: 0,
                flags: HandleFlags::Overflow,
                tag: entry.tag,
                client_byte: entry.client_byte,
            }
        } else {
            let (data_page_id, slot) = self.insert_into_data_page(value)?;
            new_inline_page = Some(data_page_id);
            HandleEntry {
                page_id: data_page_id,
                slot_index: slot,
                flags: HandleFlags::Live,
                tag: entry.tag,
                client_byte: entry.client_byte,
            }
        };

        // PREPARE: compute the new handle-table root (no install). On failure,
        // release the just-reserved inline slot so live-slot accounting stays
        // consistent with the un-installed root (overflow new-value pages are the
        // bounded commit-after-error leak class); the OLD location is untouched.
        let mut ht_freed: Vec<u64> = Vec::new();
        let saved_ht_depth = self.handle_table.depth();
        let ht_new_root =
            match self.handle_table_insert_candidate(handle, &new_entry, &mut ht_freed) {
                Ok(r) => r,
                Err(e) => {
                    if let Some(page_id) = new_inline_page {
                        self.release_data_slot(page_id);
                    }
                    return Err(e);
                }
            };

        // PREPARE: compute the OLD location's free set without applying it. For
        // Overflow this is a read-only walk of the old chain (a fallible
        // cold-page load); for Live the page id is released in the install phase.
        // A Deleted old entry is impossible here — `lookup_live` above already
        // mapped tombstones to InvalidHandle — and is rejected as CorruptPage
        // rather than carried as a third variant. On the overflow-walk failure,
        // unwind the new inline slot — the old chain is untouched (the walk frees
        // nothing).
        enum OldRelease {
            Inline(u64),
            Overflow(Vec<u64>),
        }
        let old_release = match entry.flags {
            HandleFlags::Live => OldRelease::Inline(entry.page_id),
            HandleFlags::Overflow => {
                let walked = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::collect_chain_pages(&mut cache, entry.page_id)
                };
                match walked {
                    Ok(freed) => OldRelease::Overflow(freed),
                    Err(e) => {
                        if let Some(page_id) = new_inline_page {
                            self.release_data_slot(page_id);
                        }
                        // The candidate root computed above is being discarded.
                        // If computing it grew the tree, the eager depth bump
                        // must go with it — see the depth note at the top.
                        self.handle_table.set_depth(saved_ht_depth);
                        return Err(e);
                    }
                }
            }
            HandleFlags::Deleted => {
                // I45 (ISSUES.md, 2026-05-22), same policy as `delete_inner`'s
                // Deleted arm below: `lookup_live` resolves tombstones to
                // InvalidHandle (handle_table::lookup returns None for Deleted,
                // and read.rs turns that None into the typed error), so a Deleted
                // entry arriving here means the liveness funnel itself broke —
                // an in-memory contradiction, not a caller error. This is NOT an
                // I1 reclassification: I1 forbids escalating OPERATIONAL errors
                // (CacheFull, SpillwayFull, InvalidHandle) to fatal, and this is
                // none of those.
                //
                // This arm used to be a silent no-op that retired nothing and let
                // the install below write a fresh HandleEntry over the tombstone —
                // resurrecting a deleted handle whose membership-index entry was
                // never re-added. Quietly producing a corrupt cross-map is the
                // worse of the two failure modes; `delete_inner` already decided
                // the loud one is correct.
                //
                // No PREPARE unwind is needed (matching `delete_inner`'s I45 arm,
                // which also returns without undoing its prepare): CorruptPage is
                // fatal, so `poison_on_fatal` retires the manager and nothing will
                // consume the bookkeeping an unwind would restore.
                //
                // Note this is NOT "nothing has been installed yet". The
                // handle-table install below has not run, but PREPARE already
                // called `handle_table_insert_candidate`, which unconditionally
                // `put_tree`s freemap COW growth into `current_roots.freemap_page`
                // (staging.rs) — so when that candidate drew a reuse page from a
                // non-empty committed freemap, the freemap half of `current_roots`
                // HAS advanced by the time we return here. Poisoning is what makes
                // that unobservable, not an absence of installed state.
                return Err(ChiselError::CorruptPage {
                    page_id: entry.page_id,
                });
            }
        };

        // INSTALL phase (infallible): install the NEW entry first so the handle
        // points at the new storage, THEN retire the OLD location (now genuinely
        // unreferenced). For Live, release_data_slot does R1 slot accounting
        // (freeing the page only when its last live slot goes); for Overflow, the
        // walked chain pages are queued for reclamation.
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        match old_release {
            OldRelease::Inline(page_id) => self.release_data_slot(page_id),
            OldRelease::Overflow(freed) => self.txn_freed_pages.extend_from_slice(&freed),
        }

        Ok(())
    }

    /// Delete a handle.
    ///
    /// Retires the old location (inline slot via `release_data_slot`
    /// with its R1 slot-level accounting; overflow chain by deleting
    /// every page in the chain into `txn_freed_pages`) and then asks
    /// the handle table to remove the mapping via COW. The earlier
    /// "whole-page free assumes one value per page" caveat referenced
    /// by `update`'s docstring is obsolete post-R1.
    pub fn delete(&mut self, handle: u64) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_inner(handle);
        self.poison_on_fatal(result)
    }

    fn delete_inner(&mut self, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }

        // BUG#2 atomic staging (see allocate_inner): the FORWARD map (the
        // handle-table tombstone) and the REVERSE map (membership-index removal)
        // must become durable together. We compute both candidate roots — plus
        // the fallible part of value-storage release — in a PREPARE phase that
        // never touches `current_roots`, then install everything in an
        // infallible phase. A non-fatal CacheFull/SpillwayFull mid-prepare
        // leaves the delete a complete no-op, so the reverse index can never
        // retain a member for a tombstoned handle — the stale entry that would
        // otherwise later escalate to a fatal CorruptPage.
        //
        // No in-memory depth save is needed here (unlike allocate): handle-table
        // DELETE never grows the tree, and MembershipIndex::remove writes its
        // outer_depth back only on the success path.

        // FORWARD map: compute the tombstoned root; do NOT install yet. A single
        // tree walk (I32) COWs the leaf with a tombstone and returns the
        // previous entry. Returns (root, None) — unchanged root, no COW — if the
        // handle was absent or already a tombstone; we escalate None to
        // InvalidHandle to preserve the public-API behavior.
        let mut ht_freed: Vec<u64> = Vec::new();
        let reuse = self.savepoints.is_empty();
        let mut tree = self.freemap.take_tree(&self.current_roots);
        let delete_result = {
            let mut cache = self.cache.borrow_mut();
            let mut alloc = |c: &mut PageCache| self.freemap.cow_alloc_into(c, &mut tree, reuse);
            self.handle_table.delete(
                &mut cache,
                self.current_roots.handle_table_page,
                handle,
                &mut alloc,
                &mut ht_freed,
            )
        };
        // Install freemap growth (supersedes go to structural_superseded). Done
        // BEFORE the `?` so a delete that COW'd the freemap leaf yet then errored
        // still records the extended root and returns the session set.
        self.freemap.put_tree(&mut self.current_roots, tree);
        let (ht_new_root, prev_entry) = delete_result?;
        let entry = prev_entry.ok_or(ChiselError::InvalidHandle(handle))?;

        // Stage the value-storage release. The only FALLIBLE part — walking an
        // overflow chain to collect its page ids — runs here in prepare;
        // discarding the result on a later failure is safe (the still-current
        // old entry keeps referencing the chain). The actual free-queueing /
        // slot release is deferred to the install phase below. Order vs. the
        // tombstone write does not matter for correctness — both become durable
        // (or roll back) atomically at commit.
        enum PendingRelease {
            Inline(u64),
            Overflow(Vec<u64>),
        }
        let release = match entry.flags {
            HandleFlags::Live => PendingRelease::Inline(entry.page_id),
            HandleFlags::Overflow => {
                let freed = {
                    let mut cache = self.cache.borrow_mut();
                    Overflow::collect_chain_pages(&mut cache, entry.page_id)?
                };
                PendingRelease::Overflow(freed)
            }
            HandleFlags::Deleted => {
                // I45 (ISSUES.md, 2026-05-22): a Deleted entry that `ok_or`
                // didn't catch means the in-memory state contradicts itself —
                // handle_table::delete returns None for already-tombstoned
                // handles and the ok_or above converts None into the typed
                // error, so reaching this arm signals a broken cross-module
                // contract (most likely a future refactor of delete). Surface
                // it typed rather than aborting the caller's process.
                //
                // The handle-table install below has not run, but this is not
                // "current_roots is untouched": PREPARE's candidate step already
                // `put_tree`s freemap COW growth into `current_roots.freemap_page`
                // (staging.rs), which can have advanced by the time we return.
                // CorruptPage is fatal, so poisoning is what makes the residue
                // unobservable — not an absence of installed state.
                return Err(ChiselError::CorruptPage {
                    page_id: entry.page_id,
                });
            }
        };

        // REVERSE map: compute the membership-removed root; do NOT install yet.
        // The tag comes from the tombstoned entry. Tag 0 (untagged) is never in
        // the index, so there is nothing to remove.
        let mut mi_new_root: Option<u64> = None;
        let mut idx_freed: Vec<u64> = Vec::new();
        if entry.tag != 0 {
            // Test-only injection (see `inject_membership_failure`): simulate a
            // non-fatal CacheFull at the reverse-map step so regression tests
            // exercise the REAL failure handling below. No production artifact:
            // the non-test `let res` is the only one compiled outside tests.
            #[cfg(test)]
            let res: Result<(u64, bool)> = if self.inject_membership_failure() {
                Err(ChiselError::CacheFull { limit: 0 })
            } else {
                self.membership_remove_candidate(entry.tag, handle, &mut idx_freed)
            };
            #[cfg(not(test))]
            let res: Result<(u64, bool)> =
                self.membership_remove_candidate(entry.tag, handle, &mut idx_freed);

            let (new_index_root, removed) = res?;
            // A tagged live handle always carries a reverse-index entry:
            // allocate_tagged installs it atomically (BUG#2 / PR #40) and tags
            // are immutable. So its removal must report present; a false here
            // means the forward and reverse maps diverged from some OTHER
            // source. Debug-only on purpose: a file committed while BUG#2 was
            // still live (pre-#40) could carry a real on-disk divergence, and
            // the open path gates MAJOR version only — such a legacy file must
            // stay openable and a delete of its diverged handle must remain
            // recoverable, so this never gates release builds.
            debug_assert!(
                removed,
                "membership index diverged: tagged handle {handle} (tag {}) had no reverse entry",
                entry.tag
            );
            mi_new_root = Some(new_index_root);
        }

        // INSTALL phase (infallible): tombstone + value release + reverse-map
        // removal all become visible together. The superseded handle-table
        // spine pages are queued only now, post-install, so an early prepare
        // failure leaves them referenced by the still-current old tree.
        self.current_roots.handle_table_page = ht_new_root;
        self.txn_freed_pages.append(&mut ht_freed);
        match release {
            PendingRelease::Inline(page_id) => self.release_data_slot(page_id),
            PendingRelease::Overflow(freed) => self.txn_freed_pages.extend_from_slice(&freed),
        }
        if let Some(root) = mi_new_root {
            self.current_roots.membership_index_page = root;
            self.txn_freed_pages.append(&mut idx_freed);
        }

        Ok(())
    }

    pub fn delete_tagged(&mut self, handle: u64, tag: u32) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_tagged_inner(handle, tag);
        self.poison_on_fatal(result)
    }

    fn delete_tagged_inner(&mut self, handle: u64, tag: u32) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // Lookup-then-delete: verify the tag before mutating anything, so a wrong
        // tag leaves both the chunk and the membership index untouched. The extra
        // lookup walk (delete_inner walks again) is the price of verify-before-mutate.
        // `lookup_live` rejects an absent/tombstoned handle with `InvalidHandle`
        // (I125) BEFORE the tag comparison — a dead handle never surfaces as
        // TagMismatch.
        let actual = self.lookup_live(handle)?.tag;
        if actual != tag {
            return Err(ChiselError::TagMismatch {
                handle,
                expected: tag,
                actual,
            });
        }
        self.delete_inner(handle)
    }

    /// Bounded relation drop. See `Chisel::delete_with_tag` for the full
    /// contract. Error semantics: a mid-pass `delete_inner` failure propagates
    /// `Err` and the partial `TagDropProgress` is dropped — the deleted-this-
    /// pass set is not reported. Each `delete_inner` is atomic (BUG#2 staging),
    /// so the surviving in-transaction state is consistent (rollback or commit
    /// are both safe); only the progress *reporting* is lost on error.
    pub fn delete_with_tag(&mut self, tag: u32, max: usize) -> Result<(Vec<u64>, bool)> {
        self.check_alive()?;
        let result = self.delete_with_tag_inner(tag, max);
        self.poison_on_fatal(result)
    }

    fn delete_with_tag_inner(&mut self, tag: u32, max: usize) -> Result<(Vec<u64>, bool)> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        if max == 0 {
            return Ok((Vec::new(), false));
        }
        // Bounded enumeration: ask for max+1 so the count tells us whether more
        // remain (len > max => not complete). saturating_add keeps the absurd
        // max == usize::MAX case meaning "enumerate everything" instead of
        // wrapping to 0 — which would enumerate nothing and falsely report the
        // tag complete. The members snapshot is taken BEFORE the deletions, then
        // each is deleted via delete_inner (which removes it from the index and
        // frees its chunk).
        //
        // The snapshot must be MATERIALIZED into an owned Vec, not a live
        // iterator: each delete_inner COWs the membership-index root (via
        // membership_remove_candidate), so walking the index while deleting from
        // it would descend a tree being rewritten underneath the walk. Collecting
        // first decouples enumeration from the mutation it drives.
        let members = {
            let root = self.current_roots.membership_index_page;
            let mut cache = self.cache.borrow_mut();
            self.membership_index.handles_for_tag_bounded(
                &mut cache,
                root,
                tag,
                max.saturating_add(1),
            )?
        };
        let complete = members.len() <= max;
        let take: Vec<u64> = members.into_iter().take(max).collect();
        for &h in &take {
            self.delete_inner(h)?;
        }
        Ok((take, complete))
    }

    /// Delete many handles in a single transaction.
    ///
    /// Today: this is a loop over `delete_inner`. After PR-A's fusion
    /// (I32), each delete walks the handle table once per handle. For
    /// dense delete patterns (many handles in the same leaf), a
    /// per-leaf batched implementation would walk once per leaf
    /// instead — that's tracked as I33 in ISSUES.md, deferred until
    /// a workload demonstrates the win is worth the complexity.
    ///
    /// Error semantics: on the first error the loop stops and returns
    /// the error. Handles deleted before the failure remain marked
    /// for deletion in `current_roots`, so the caller can choose
    /// between `rollback()` (abandon the whole batch) or `commit()`
    /// (keep the partial work).
    pub fn delete_many(&mut self, handles: &[u64]) -> Result<()> {
        self.check_alive()?;
        let result = self.delete_many_inner(handles);
        self.poison_on_fatal(result)
    }

    fn delete_many_inner(&mut self, handles: &[u64]) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        // See I33 in ISSUES.md for the deferred per-leaf batching work.
        for &handle in handles {
            self.delete_inner(handle)?;
        }
        Ok(())
    }
}
