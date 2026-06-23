//! transaction::config — runtime knobs: cache / spillway byte caps and
//! the drain-insertion policy. Split out of `transaction.rs` verbatim; see
//! the parent module for the type and fields.

use super::*;

impl TransactionManager {
    pub fn set_cache_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.borrow_mut().set_cache_max_bytes(bytes)
    }

    pub fn set_spillway_max_bytes(&mut self, bytes: u64) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        self.cache.borrow_mut().set_spillway_max_bytes(bytes)
    }

    pub fn set_drain_insertion(&mut self, policy: crate::DrainInsertion) -> Result<()> {
        self.check_alive()?;
        if self.active_txn {
            return Err(ChiselError::TransactionInProgress);
        }
        // I40: PageCache::set_drain_insertion is now infallible (returns
        // `()`). The poison + active-txn checks above are the real
        // failure modes; we promote PageCache's `()` to `Ok(())` here.
        self.cache.borrow_mut().set_drain_insertion(policy);
        Ok(())
    }
}
