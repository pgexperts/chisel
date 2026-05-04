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

        // PR 8 fairness fix: on macOS, plain fsync() flushes to OS write
        // buffer but not to the disk's write cache. Chisel's sync_all uses
        // fcntl(F_FULLFSYNC) which is durable through the disk cache;
        // without the equivalent in SQLite, sqlite-strict on macOS is
        // ~3 orders of magnitude faster than chisel-strict, which is a
        // measurement artifact, not a real performance difference.
        // PRAGMA fullfsync=ON makes SQLite call F_FULLFSYNC on every
        // sync. Linux ignores the pragma (its fsync() already flushes
        // through). Strict mode only — Unsafe is the speed-over-safety
        // dial; pulling it back via fullfsync would defeat its purpose.
        if matches!(durability, DurabilityMode::Strict) {
            conn.execute_batch("PRAGMA fullfsync = ON;")?;
        }

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
            // Diagnostic path for the snapshot-restore intermittent: if the
            // working copy is genuinely missing rows, row_count < expected
            // and max_id < id; if the file is intact but somehow returning
            // 0 affected rows, max_id >= id but row_count == expected.
            let row_count: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM chisel_bench;", [], |r| r.get(0))
                .unwrap_or(-1);
            let max_id: i64 = self
                .conn
                .query_row("SELECT COALESCE(MAX(id), -1) FROM chisel_bench;", [], |r| {
                    r.get(0)
                })
                .unwrap_or(-1);
            return Err(format!(
                "identifier {} not found in chisel_bench (path={:?}, rows={}, max_id={})",
                id.0, self.path, row_count, max_id
            )
            .into());
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

    // WAL mode keeps committed data in the `-wal` sibling until
    // checkpointed. Copying just the `.db` file (as the bench harness
    // does at snapshot-restore boundaries) leaves WAL-resident rows
    // missing from the copy — surfacing as "database disk image is
    // malformed" or "identifier not found" depending on which page the
    // next operation lands on. The earlier wal_checkpoint(TRUNCATE)
    // implementation didn't reliably eliminate this: that PRAGMA
    // returns (busy, log_frames, checkpointed_frames), and a partial
    // checkpoint (busy != 0 or checkpointed < log) is reported only
    // through those return values — execute_batch silently discards
    // them, so a partial drain looked successful. At ~25 MB of
    // accumulated WAL pages the partial-completion case became
    // observable in the bench (sqlite-strict/{1MB,2KB} cells, both
    // ~25 MB raw payload, both intermittently failing).
    //
    // Switching journal_mode to DELETE is the bulletproof variant: the
    // PRAGMA refuses to return until the WAL is fully drained into the
    // main .db AND the -wal/-shm siblings are deleted. The .db file
    // becomes a genuinely self-contained rollback-journal-mode database
    // that file-copies cleanly. Each cell-runner re-enters WAL mode via
    // open_file's existing PRAGMA on open, so no behavioral change
    // downstream of this call.
    //
    // synchronous=FULL is set first so the DELETE switch's final I/O is
    // fsync'd even when the engine was opened in Unsafe mode
    // (synchronous=OFF) — otherwise the now-self-contained .db could
    // still race OS buffer flush vs. the bench's std::fs::copy.
    //
    // Not placed in `Drop` because per-iteration bench cells open and
    // drop engines inside the timed region; an always-on Drop variant
    // adds ~80% time to every snapshot-restore measurement.
    //
    // Residual transient (as of 2026-05-01): with this fix, the bench
    // harness still failed ~1 in 5 full --quick runs at one of the
    // ~25 MB sqlite-strict cells (1MB×25 or 12500×256B), surfacing as
    // either "database disk image is malformed" on a working-copy read
    // or "identifier not found" on a working-copy update. Down from
    // ~50% with the prior wal_checkpoint(TRUNCATE)-only variant. We
    // could not reproduce in isolation (focused tests, single-group
    // bench runs, single-cell bench runs all clean across many tries),
    // suggesting the failure requires the full bench's accumulated
    // tempfile / OS page-cache state. The diagnostic block in
    // SqliteEngine::update below dumps row count + max id on the
    // missing-identifier path so the next observed failure has
    // actionable context. If this becomes more frequent, candidates
    // worth investigating include: an explicit VACUUM before the mode
    // switch; rusqlite's native wal_checkpoint API (which exposes the
    // partial-completion return values execute_batch hides); or a
    // sqlite3_open_v2 with SQLITE_OPEN_NOMUTEX to rule out internal
    // sync state.
    fn flush_for_snapshot(&mut self) -> EngineResult<()> {
        self.conn.execute_batch("PRAGMA synchronous=FULL;")?;
        let new_mode: String = self
            .conn
            .query_row("PRAGMA journal_mode = DELETE;", [], |row| row.get(0))?;
        if new_mode != "delete" {
            return Err(format!(
                "flush_for_snapshot: journal_mode switch returned {new_mode:?}, expected \"delete\" \
                 (likely an unexpected concurrent reader)"
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn strict_mode_sets_fullfsync_pragma() {
        let tmp = NamedTempFile::new().unwrap();
        let engine = SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Strict).unwrap();
        // Query the pragma value back. SQLite returns it as an integer:
        // 1 = ON, 0 = OFF.
        let value: i64 = engine
            .conn
            .query_row("PRAGMA fullfsync;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1, "Strict mode must enable fullfsync");
    }

    #[test]
    fn unsafe_mode_does_not_set_fullfsync_pragma() {
        let tmp = NamedTempFile::new().unwrap();
        let engine = SqliteEngine::open_file(tmp.path(), 64, DurabilityMode::Unsafe).unwrap();
        let value: i64 = engine
            .conn
            .query_row("PRAGMA fullfsync;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 0, "Unsafe mode must NOT enable fullfsync");
    }
}
