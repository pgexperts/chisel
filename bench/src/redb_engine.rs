// RedbEngine — Engine trait impl backed by redb.
//
// Schema: a single table mapping caller-generated monotonic u64 keys
// to byte-blob values. The harness owns the key-allocation policy
// (next_id starts from max_existing_key + 1 on open, increments
// monotonically, never reuses) so that identifier semantics match
// Chisel's "handles never reused after delete" promise — see the
// Engine::allocate doc comment.
//
// Transaction state lives on the struct as Option<WriteTransaction>.
// redb 2.x's WriteTransaction is 'static-suitable (it holds Arc to
// internal state) so storing it directly works without lifetime
// gymnastics.

use crate::engine::{DurabilityMode, Engine, EngineResult, Identifier};
use chisel::stats::ChiselCounters;
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("chisel_bench");

const PAGE_SIZE: usize = 8192;

pub struct RedbEngine {
    db: Database,
    path: PathBuf,
    next_id: u64,
    durability: DurabilityMode,
    active_tx: Option<redb::WriteTransaction>,
}

impl RedbEngine {
    /// Open or create a file-backed redb database.
    ///
    /// `cache_size_pages` matches the harness convention: pages of
    /// 8 KB. redb's API takes bytes; we multiply.
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let cache_bytes = cache_size_pages.max(1) * PAGE_SIZE;
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let next_id = recover_next_id(&db)?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
            next_id,
            durability,
            active_tx: None,
        })
    }
}

/// Find the largest existing key + 1, or 0 if the table is empty or
/// missing. Called once at open time to seed the monotonic key
/// allocator. Cost: one read transaction, one table iter-back.
fn recover_next_id(db: &Database) -> EngineResult<u64> {
    let read_tx = db.begin_read()?;
    match read_tx.open_table(TABLE) {
        Ok(table) => {
            // last() returns the largest key under redb's u64 ordering
            // (big-endian byte order, which matches numeric order).
            match table.last()? {
                Some((key, _)) => Ok(key.value() + 1),
                None => Ok(0),
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

impl Engine for RedbEngine {
    fn begin(&mut self) -> EngineResult<()> {
        if self.active_tx.is_some() {
            return Err("transaction already active".into());
        }
        let mut tx = self.db.begin_write()?;
        let durability = match self.durability {
            DurabilityMode::Strict => Durability::Immediate,
            DurabilityMode::Unsafe => Durability::Eventual,
        };
        tx.set_durability(durability);
        self.active_tx = Some(tx);
        Ok(())
    }

    fn commit(&mut self) -> EngineResult<()> {
        let tx = self.active_tx.take().ok_or("no active transaction")?;
        tx.commit()?;
        Ok(())
    }

    fn rollback(&mut self) -> EngineResult<()> {
        let tx = self.active_tx.take().ok_or("no active transaction")?;
        tx.abort()?;
        Ok(())
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        let id = self.next_id;
        {
            let tx = self.active_tx.as_ref().ok_or("no active transaction")?;
            let mut table = tx.open_table(TABLE)?;
            table.insert(id, value)?;
        }
        self.next_id += 1;
        Ok(Identifier(id))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        if let Some(tx) = &self.active_tx {
            let table = tx.open_table(TABLE)?;
            let value = table.get(id.0)?.ok_or("identifier not found")?;
            Ok(value.value().to_vec())
        } else {
            let read_tx = self.db.begin_read()?;
            let table = read_tx.open_table(TABLE)?;
            let value = table.get(id.0)?.ok_or("identifier not found")?;
            Ok(value.value().to_vec())
        }
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        let tx = self.active_tx.as_ref().ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        // redb's insert is upsert; we don't need a separate update path.
        // Verify the key exists first to match the trait's semantic
        // (update on a non-existent identifier returns Err).
        if table.get(id.0)?.is_none() {
            return Err("identifier not found".into());
        }
        table.insert(id.0, value)?;
        Ok(())
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        let tx = self.active_tx.as_ref().ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        let removed = table.remove(id.0)?;
        if removed.is_none() {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        let tx = self.active_tx.as_ref().ok_or("no active transaction")?;
        let mut table = tx.open_table(TABLE)?;
        for id in ids {
            let removed = table.remove(id.0)?;
            if removed.is_none() {
                return Err("identifier not found".into());
            }
        }
        Ok(())
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        Ok(None)
    }
}
