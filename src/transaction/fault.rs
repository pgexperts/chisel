//! Test-only fault injection consolidated off the production TransactionManager
//! (review 2026-06-22 SMELL #4). Each Cell arms a one-shot or countdown failure
//! at a precise commit-protocol divergence window; see the BUG#2 staging tests.
//!
//! - `fail_next_membership_op`: the NEXT reverse-map (membership-index) update in
//!   `allocate_inner`/`delete_inner` returns a non-fatal `CacheFull` BEFORE touching
//!   the index, simulating a mid-operation resource-exhaustion strike at the
//!   forward/reverse divergence window. Mirrors `force_poison_for_test` — no
//!   production code.
//! - `fail_next_handle_table_op`: companion for the FORWARD step — the next
//!   `allocate_inner` handle-table insert returns a non-fatal `CacheFull`, exercising
//!   the prepare-abort/unwind path of the step carrying the eager depth bump
//!   (HandleTable::grow).
//! - `fail_next_update_value_write`: for `update_inner` — the next update returns a
//!   non-fatal `CacheFull` at the NEW-value-write step (the first fallible step with
//!   the fix; pre-fix it landed AFTER the old location was freed), proving the old
//!   value is not prematurely freed before the new entry installs.
//! - `fail_membership_op_after`: countdown variant of `fail_next_membership_op` for
//!   multi-delete passes (e.g. delete_with_tag) — when set to K, the Kth subsequent
//!   membership-index op fails (the first K-1 succeed), so a test can fail a LATER
//!   delete in a loop while earlier deletes commit first. 0 disables.
use std::cell::Cell;

#[derive(Default)]
pub(super) struct FaultInjector {
    pub fail_next_membership_op: Cell<bool>,
    pub fail_next_handle_table_op: Cell<bool>,
    pub fail_next_update_value_write: Cell<bool>,
    pub fail_membership_op_after: Cell<u32>,
}
