//! transaction::named_roots — named-root table (ISSUES.md F2):
//! name encoding/validation plus set / get / clear (+ their `_inner`
//! cores). Split out of `transaction.rs` verbatim; see the parent module.

use super::*;

impl TransactionManager {
    // --- Named roots (ISSUES.md F2) ---
    //
    // The named-root table lives inside the superblock (see
    // `superblock::NamedRoot`). Modifications update
    // `current_roots.named_roots` in memory; on commit that array is
    // copied into the new Superblock and fsync'd along with the rest.
    // On rollback or `rollback_to`, the usual snapshot restore reverts
    // named roots alongside the handle-table root — no extra plumbing.
    //
    // Name validation is intentionally strict: names must be non-empty,
    // must fit in NAMED_ROOT_NAME_LEN bytes, must not contain NUL
    // (because NUL is the "empty slot" sentinel), and must be valid
    // UTF-8 at the API boundary. Names are compared byte-for-byte after
    // validation; the fixed 24-byte buffer is NUL-padded.

    /// Validate a root name and return its byte form, padded to
    /// NAMED_ROOT_NAME_LEN with trailing NULs. Returns `InvalidRootName`
    /// on any violation.
    fn encode_root_name(name: &str) -> Result<[u8; NAMED_ROOT_NAME_LEN]> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > NAMED_ROOT_NAME_LEN {
            return Err(ChiselError::InvalidRootName);
        }
        if bytes.contains(&0) {
            return Err(ChiselError::InvalidRootName);
        }
        let mut encoded = [0u8; NAMED_ROOT_NAME_LEN];
        encoded[..bytes.len()].copy_from_slice(bytes);
        Ok(encoded)
    }

    /// Bind `name` to `handle` in the named-root table. If `name` already
    /// exists, its handle is overwritten. If it doesn't exist and the
    /// table has no empty slots, returns `RootNameTableFull`. Requires an
    /// active transaction and becomes durable on commit; reverts on
    /// rollback/rollback_to.
    pub fn set_root_name(&mut self, name: &str, handle: u64) -> Result<()> {
        self.check_alive()?;
        let result = self.set_root_name_inner(name, handle);
        self.poison_on_fatal(result)
    }

    fn set_root_name_inner(&mut self, name: &str, handle: u64) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let encoded = Self::encode_root_name(name)?;

        // First pass: update in place if the name already exists.
        for entry in self.current_roots.named_roots.iter_mut() {
            if !entry.is_empty() && entry.name == encoded {
                entry.handle = handle;
                return Ok(());
            }
        }
        // Second pass: install in the first empty slot.
        for entry in self.current_roots.named_roots.iter_mut() {
            if entry.is_empty() {
                entry.name = encoded;
                entry.handle = handle;
                return Ok(());
            }
        }
        Err(ChiselError::RootNameTableFull)
    }

    /// Look up a named root. Returns `Ok(None)` if the name is not bound.
    /// Reads see the transactional view: inside an active transaction,
    /// pending `set_root_name` / `clear_root_name` changes are visible;
    /// outside a transaction, reads the last durably committed table.
    ///
    /// Takes `&self` — named-root reads are semantically read-only.
    pub fn get_root_name(&self, name: &str) -> Result<Option<u64>> {
        self.check_alive()?;
        let result = self.get_root_name_inner(name);
        self.poison_on_fatal(result)
    }

    fn get_root_name_inner(&self, name: &str) -> Result<Option<u64>> {
        let encoded = Self::encode_root_name(name)?;
        let table = if self.active_txn {
            &self.current_roots.named_roots
        } else {
            &self.committed_roots.named_roots
        };
        for entry in table.iter() {
            if !entry.is_empty() && entry.name == encoded {
                return Ok(Some(entry.handle));
            }
        }
        Ok(None)
    }

    /// Remove a named root. No-op if the name is not bound (returns Ok).
    /// Requires an active transaction. Becomes durable on commit;
    /// reverts on rollback/rollback_to.
    pub fn clear_root_name(&mut self, name: &str) -> Result<()> {
        self.check_alive()?;
        let result = self.clear_root_name_inner(name);
        self.poison_on_fatal(result)
    }

    fn clear_root_name_inner(&mut self, name: &str) -> Result<()> {
        if !self.active_txn {
            return Err(ChiselError::NoActiveTransaction);
        }
        let encoded = Self::encode_root_name(name)?;
        for entry in self.current_roots.named_roots.iter_mut() {
            if !entry.is_empty() && entry.name == encoded {
                *entry = NamedRoot::EMPTY;
                return Ok(());
            }
        }
        Ok(())
    }
}
