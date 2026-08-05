//! transaction::commit — the 3-fsync commit protocol (I28 pre-drain flush ->
//! freemap persist -> data flush -> superblock write+fsync -> roots promotion).
//! The data-fsync-before-superblock-fsync ordering and the I18 allocate-before-
//! merge invariant are what make shadow paging crash-safe; this sequence is moved
//! out of lifecycle.rs VERBATIM. Stateless: operates over the manager's parts via
//! CommitCtx. The poison-on-error wrapper stays on TransactionManager::commit.

use super::*;

/// Borrows of the exactly-twelve pieces of `TransactionManager` state the commit
/// protocol touches, bundled so `commit_inner` can stay a thin caller while the
/// load-bearing sequence lives here. All fields are distinct manager fields (plus
/// the shared `&RefCell` for the cache), so the borrow checker accepts the
/// simultaneous distinct-field borrows the caller constructs.
pub(super) struct CommitCtx<'a> {
    pub cache: &'a std::cell::RefCell<PageCache>,
    pub savepoints: &'a mut Vec<Savepoint>,
    pub txn_freed_pages: &'a mut Vec<u64>,
    pub freemap: &'a mut freemap::FreemapRecycle,
    pub packer: &'a mut packing::SlotPacker,
    pub committed_roots: &'a mut Roots,
    pub current_roots: &'a mut Roots,
    pub txn_counter: &'a mut u64,
    pub active_txn: &'a mut bool,
    pub superblock_count: u32,
    /// Cipher for an encrypted database; `None` for plaintext. Needed here so the
    /// superblock body (sensitive fields) is sealed before being written to disk.
    pub cipher: Option<&'a crate::crypto::PageCipher>,
    /// The key-slot table stamped into the on-disk superblock. Carried through
    /// the session unchanged; None for plaintext DBs.
    pub crypto_header: Option<&'a crate::superblock::CryptoHeader>,
}

pub(super) fn run_commit(ctx: &mut CommitCtx<'_>) -> Result<()> {
    // I27: flatten every still-active savepoint's `freed_pages`
    // back into `txn_freed_pages` before persist_freemap consumes
    // it. savepoint_inner moves `txn_freed_pages` INTO the
    // savepoint record (via std::mem::take), so any frees that
    // happened before a still-unreleased savepoint otherwise get
    // dropped on the floor when step 5 calls `savepoints.clear()`
    // — a permanent freemap leak for the "commit with savepoint
    // active" pattern. Mirrors `release_inner`'s merge but applied
    // across the full stack. We take the lists out of the
    // savepoints (rather than iterating by reference) so the
    // savepoints hold no stale `freed_pages` if we ever change
    // step 5 to drain instead of clear; current code is equivalent
    // either way.
    for sp in ctx.savepoints.iter_mut() {
        ctx.txn_freed_pages.append(&mut sp.freed_pages);
    }

    // I28: drain the page cache BEFORE persist_freemap runs. Without
    // this, `persist_freemap`'s own freemap-page allocation
    // (`structural_extend` → `new_page`) can trip
    // `maybe_evict`'s spill-or-CacheFull decision (every existing entry
    // dirty, nothing evictable, and either spillway disabled or full)
    // and return `ChiselError::CacheFull` or `ChiselError::SpillwayFull`.
    // The CacheFull variant is operational-by-design (I19 docs: "caller
    // recovers by
    // committing or rolling back"), but commit's poison wrapper fires
    // on any error once the protocol has started — demoting an
    // operational signal to fatal for a caller who has no legal
    // action left (commit is precisely what failed). Pre-draining
    // clears every dirty pin so the ceiling is reachable via normal
    // eviction when persist_freemap itself allocates. Cost: one
    // extra fsync on every commit. That is consistent with the
    // project's explicit "durability over performance" posture —
    // the alternative reclassifies CacheFull as fatal inside commit,
    // which is both more surprising and harder to document cleanly.
    //
    // Ordering note: this flush is safe to do before persist_freemap.
    // The shadow-paging invariant requires "new-freemap-page durable
    // before superblock" (step 1's flush does that). The pre-drain
    // only affects user-dirty pages, which are already part of the
    // transaction's durable write set — just flushed earlier. The
    // subsequent step 1 flush handles the one new freemap page
    // persist_freemap adds.
    ctx.cache.borrow_mut().flush()?;

    // Step 0 (ISSUES.md R2 / I11): persist the freemap tree. This marks
    // `txn_freed_pages` (plus the prior commit's deferred structural frees)
    // free in a COW of the committed tree and updates
    // `current_roots.{freemap_page, freemap_depth}`. Runs BEFORE the main
    // flush so the new freemap pages join the same durable write set as all
    // other dirty data pages.
    //
    // Persist's own borrow lives in this scope so it drops before the held
    // step-1-through-4 borrow acquired below: a second `borrow_mut` on the
    // RefCell while the first is still live would panic. `current_roots`
    // (mut), `freemap` (mut), and `txn_freed_pages` (read) are disjoint, so
    // `txn_freed_pages` passes by `&` with no take/restore dance.
    {
        let mut cache = ctx.cache.borrow_mut();
        ctx.freemap
            .persist(&mut cache, ctx.current_roots, ctx.txn_freed_pages)?;
    }

    // Hold one RefMut for the remaining steps. Dropping and
    // re-borrowing between steps would be semantically identical
    // but noisier.
    let mut cache = ctx.cache.borrow_mut();

    // Step 1: Flush all dirty pages (PageCache::flush internally fsyncs).
    // After this, every page the new superblock will reference is on disk.
    cache.flush()?;

    // Step 2: Build the new superblock. Bumping txn_counter here both makes
    // it outrank the current superblock on recovery AND (via parity) picks
    // the target slot in step 3.
    //
    // I119 (ISSUES.md, 2026-06-21): checked, not `+= 1`. A wrapped counter
    // would corrupt `Superblock::select`'s "highest counter wins" (release
    // wrap to 0) — far worse than the loud, controlled panic here.
    //
    // The original rationale for the `expect` was "overflow needs 2^64 commits,
    // so it is structurally unreachable". That was WRONG, and worth spelling out
    // because it is the sentence a future maintainer would trust: the counter is
    // not only produced by this binary's increments, it is also READ FROM THE
    // FILE at open (`recovery.rs`, `txn_counter: sb.txn_counter`). A superblock
    // forged with `txn_counter = u64::MAX` and a recomputed XXH3 checksum made
    // the panic fire on the first commit after open.
    //
    // What makes it unreachable now is an actual invariant rather than an
    // argument about counting: `Superblock::validate` rejects any slot whose
    // counter exceeds MAX_TXN_COUNTER, so a file that opened at all has at least
    // TXN_COUNTER_RESERVE (2^32) increments of headroom. Exhausting that takes
    // 4.3 billion commits in one session, since a reopen re-validates.
    // The `expect` therefore stays: it now documents a guarded invariant rather
    // than asserting an unguarded hope.
    *ctx.txn_counter = ctx
        .txn_counter
        .checked_add(1)
        .expect("txn_counter overflowed u64 (2^64 commits) — unreachable");
    let total_pages = cache.file_page_count()?;
    let sb = Superblock {
        magic: page::MAGIC,
        // Encrypted DBs use MAJOR=2 so an old binary rejects them.
        format_version: if ctx.crypto_header.is_some() {
            page::format_version_encrypted()
        } else {
            page::FORMAT_VERSION
        },
        txn_counter: *ctx.txn_counter,
        root_handle_table_page: ctx.current_roots.handle_table_page,
        root_freemap_page: ctx.current_roots.freemap_page,
        total_pages,
        next_handle: ctx.current_roots.next_handle,
        page_size: PAGE_SIZE as u32,
        named_roots: ctx.current_roots.named_roots,
        // R4: every slot records the current N so open-time
        // recovery can discover it from the winning slot without
        // external hints.
        superblock_count: ctx.superblock_count,
        root_membership_index_page: ctx.current_roots.membership_index_page,
        // Freemap tree depth, paired with root_freemap_page. 0 = today's
        // single-leaf format; grows as the tree deepens.
        freemap_depth: ctx.current_roots.freemap_depth,
        encryption: ctx.crypto_header.copied(),
    };
    // For encrypted DBs, seal the body (sensitive fields) under the session DEK.
    // The crypto-header (key-slot table) is written verbatim in cleartext.
    let buf = match (ctx.cipher, ctx.crypto_header) {
        (Some(cipher), Some(_)) => sb.serialize_encrypted(cipher),
        _ => sb.serialize(),
    };
    // Step 3: Write to the INACTIVE slot. For N superblock slots,
    // the slot is `txn_counter % N` — a round-robin that always
    // targets the stalest slot. With N=2 this is the parity
    // alternation from the original layout; with N>=3 it extends
    // to true round-robin. The currently-active slot (and every
    // other non-target slot) is never touched, so a torn write
    // here can only damage the new superblock, never the N-1
    // last-known-good ones.
    let inactive = *ctx.txn_counter % ctx.superblock_count as u64;
    // Superblock-at-8232: for an encrypted DB, stride is ENC_PAGE_SIZE=8232 so
    // write_page (which asserts stride==PAGE_SIZE) would panic. The superblock
    // image is 8192 bytes (plaintext header + Phase-2 encrypted body) and is NOT
    // PageCipher-sealed; Phase 2 already encrypts its body. We write it into a
    // zero-padded ENC_PAGE_SIZE unit so the slot occupies the same 8232-byte
    // region on disk that data pages use, then write via write_page_unit.
    // For plaintext DBs (stride==PAGE_SIZE), write_page still works fine.
    if ctx.cipher.is_some() {
        use crate::crypto::ENC_PAGE_SIZE;
        let mut unit = [0u8; ENC_PAGE_SIZE];
        unit[..buf.len()].copy_from_slice(&buf);
        cache.io_mut().write_page_unit(inactive, &unit)?;
    } else {
        cache.io_mut().write_page(inactive, &buf)?;
    }
    // Step 4: Durability linearization point. Until this fsync returns the
    // transaction is not crash-safe; after it returns the new state is
    // observable on recovery.
    cache.io_mut().fsync()?;

    // Step 5: Promote in-memory state. Only now is the txn officially committed.
    *ctx.committed_roots = ctx.current_roots.clone();
    ctx.committed_roots.total_pages = total_pages;
    // The committed freemap tree advances automatically: its {root, depth}
    // ride in current_roots, promoted into committed_roots just above. No
    // separate in-memory freemap copy to advance.
    // R1: promote the live-slot counts. The cursor is per-transaction
    // and gets reset for the next begin().
    ctx.packer.commit();
    *ctx.active_txn = false;
    ctx.savepoints.clear();
    // txn_freed_pages were already marked free in the new committed freemap
    // tree by persist_freemap; clear the vector now that it's done its job.
    ctx.txn_freed_pages.clear();
    // Clear the freemap session set (every page COW'd this transaction is now
    // committed) and promote the structural recycle for the next transaction:
    // this commit's supersedes + the unconsumed reuse remainder become next
    // transaction's `pending_structural_frees`. See `FreemapRecycle::commit`.
    ctx.freemap.commit();

    Ok(())
}
