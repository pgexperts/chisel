// Focused reproducer for the malformed-image failure observed at
// update-1pertx/sqlite-strict/1MB during PR 4b bench runs. Mirrors the
// cell-runner pattern: populate_snapshot, then std::fs::copy + open +
// update.

use chisel_bench::engine::{DurabilityMode, Engine, Identifier};
use chisel_bench::runner::{populate_snapshot, EngineMode, CACHE_SIZE_PAGES};
use chisel_bench::sqlite_engine::SqliteEngine;
use rusqlite::Connection;
use tempfile::NamedTempFile;

fn integrity_check(path: &std::path::Path) -> String {
    let conn = Connection::open(path).unwrap();
    conn.query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
        .unwrap()
}

#[test]
fn sqlite_snapshot_restore_then_update_1mb_aggressive() {
    // Mimic the bench loop: ChiselStrict, RedbStrict, RedbUnsafe, SqliteStrict
    // all populate at 1MB before the cell-runner iterates the SqliteStrict
    // snapshot.
    let _chisel_snap = populate_snapshot(EngineMode::ChiselStrict, 1_048_576, 25).unwrap();
    let _redb_strict_snap = populate_snapshot(EngineMode::RedbStrict, 1_048_576, 25).unwrap();
    let _redb_unsafe_snap = populate_snapshot(EngineMode::RedbUnsafe, 1_048_576, 25).unwrap();
    let snap = populate_snapshot(EngineMode::SqliteStrict, 1_048_576, 25).unwrap();

    let snap_check = integrity_check(snap.path());
    assert_eq!(snap_check, "ok", "snapshot integrity check failed");

    // Many iterations to mimic criterion's --quick sample count.
    for iter in 0..20 {
        let working = NamedTempFile::new().unwrap();
        std::fs::copy(snap.path(), working.path()).unwrap();

        let working_check = integrity_check(working.path());
        assert_eq!(
            working_check, "ok",
            "iter {iter}: working copy integrity check failed"
        );

        let mut engine =
            SqliteEngine::open_file(working.path(), CACHE_SIZE_PAGES, DurabilityMode::Strict)
                .expect("open after copy");

        engine.begin().expect("begin");
        let target_id = Identifier(snap.ids()[iter % snap.ids().len()]);
        let payload = vec![iter as u8; 1_048_576];
        engine.update(target_id, &payload).unwrap_or_else(|e| {
            panic!("iter {iter}: update on snapshot copy failed: {e:?}");
        });
        engine.commit().expect("commit");
    }
}
