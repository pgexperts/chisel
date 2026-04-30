// SqliteEngine — Engine trait impl backed by rusqlite.
//
// Schema: chisel_bench(id INTEGER PRIMARY KEY AUTOINCREMENT, value BLOB).
// AUTOINCREMENT is load-bearing — it suppresses SQLite's default
// rowid-reuse-on-delete behavior, matching Chisel's handle-stability
// promise (see Engine::allocate doc comment).
//
// We don't use rusqlite's Transaction wrapper because it borrows
// &mut Connection — can't hold across separate &mut self calls
// without lifetime gymnastics. Instead we run BEGIN/COMMIT/ROLLBACK
// as raw SQL via execute_batch; transaction state is a simple bool.

use crate::engine::{DurabilityMode, Engine, EngineResult, Identifier};
use chisel::stats::ChiselCounters;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct SqliteEngine {
    conn: Connection,
    path: PathBuf,
    #[allow(dead_code)] // currently unused but preserved for symmetry with RedbEngine
    durability: DurabilityMode,
    active_tx: bool,
}

impl SqliteEngine {
    /// Open or create a file-backed SQLite database.
    ///
    /// `cache_size_pages` matches the harness convention: pages of
    /// 8 KB. SQLite's PRAGMA cache_size = -<KB> takes KB; we multiply.
    pub fn open_file(
        path: &Path,
        cache_size_pages: usize,
        durability: DurabilityMode,
    ) -> EngineResult<Self> {
        let conn = Connection::open(path)?;

        let cache_kb = cache_size_pages.max(1) * 8;
        conn.execute_batch(&format!(
            "PRAGMA cache_size = -{cache_kb}; \
             PRAGMA journal_mode = WAL;"
        ))?;

        let synchronous = match durability {
            DurabilityMode::Strict => "FULL",
            DurabilityMode::Unsafe => "OFF",
        };
        conn.execute_batch(&format!("PRAGMA synchronous = {synchronous};"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chisel_bench ( \
                id    INTEGER PRIMARY KEY AUTOINCREMENT, \
                value BLOB    NOT NULL \
            )",
        )?;

        Ok(Self {
            conn,
            path: path.to_path_buf(),
            durability,
            active_tx: false,
        })
    }
}

impl Engine for SqliteEngine {
    fn begin(&mut self) -> EngineResult<()> {
        if self.active_tx {
            return Err("transaction already active".into());
        }
        self.conn.execute_batch("BEGIN")?;
        self.active_tx = true;
        Ok(())
    }

    fn commit(&mut self) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        self.conn.execute_batch("COMMIT")?;
        self.active_tx = false;
        Ok(())
    }

    fn rollback(&mut self) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        self.conn.execute_batch("ROLLBACK")?;
        self.active_tx = false;
        Ok(())
    }

    fn allocate(&mut self, value: &[u8]) -> EngineResult<Identifier> {
        // SQLite is autocommit by default — without an explicit BEGIN, this
        // INSERT would silently persist. The trait promises that mutations
        // outside a transaction return Err; enforce that here. Same guard
        // pattern repeats on update / delete / delete_many.
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        self.conn.execute(
            "INSERT INTO chisel_bench (value) VALUES (?)",
            rusqlite::params![value],
        )?;
        // SQLite's rowid is i64 native; AUTOINCREMENT keeps it
        // positive and growing, well below i64::MAX in practice.
        let rowid = self.conn.last_insert_rowid();
        Ok(Identifier(rowid as u64))
    }

    fn read(&self, id: Identifier) -> EngineResult<Vec<u8>> {
        let row: Vec<u8> = self.conn.query_row(
            "SELECT value FROM chisel_bench WHERE id = ?",
            rusqlite::params![id.0 as i64],
            |row| row.get(0),
        )?;
        Ok(row)
    }

    fn update(&mut self, id: Identifier, value: &[u8]) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        let n = self.conn.execute(
            "UPDATE chisel_bench SET value = ? WHERE id = ?",
            rusqlite::params![value, id.0 as i64],
        )?;
        if n == 0 {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete(&mut self, id: Identifier) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        let n = self.conn.execute(
            "DELETE FROM chisel_bench WHERE id = ?",
            rusqlite::params![id.0 as i64],
        )?;
        if n == 0 {
            return Err("identifier not found".into());
        }
        Ok(())
    }

    fn delete_many(&mut self, ids: &[Identifier]) -> EngineResult<()> {
        if !self.active_tx {
            return Err("no active transaction".into());
        }
        let mut stmt = self.conn.prepare("DELETE FROM chisel_bench WHERE id = ?")?;
        for id in ids {
            let n = stmt.execute(rusqlite::params![id.0 as i64])?;
            if n == 0 {
                return Err("identifier not found".into());
            }
        }
        Ok(())
    }

    fn file_size_bytes(&self) -> EngineResult<u64> {
        // SQLite in WAL mode keeps a -wal journal and -shm shared-memory
        // index alongside the main file. Honest "size on disk" sums all
        // three when present.
        let mut total = std::fs::metadata(&self.path)?.len();
        for suffix in ["-wal", "-shm"] {
            let mut sibling = self.path.clone().into_os_string();
            sibling.push(suffix);
            if let Ok(m) = std::fs::metadata(&sibling) {
                total += m.len();
            }
        }
        Ok(total)
    }

    fn internal_counters(&self) -> EngineResult<Option<ChiselCounters>> {
        Ok(None)
    }
}
