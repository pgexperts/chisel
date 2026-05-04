// overflow.rs — integration tests for Overflow (multi-page large-value storage).
//
// Tests are run against both file-backed and memory-backed PageIo via
// `dual_backing_test!`. The harness's `Backing` enum drives the choice;
// a local `make_cache` helper converts it to a `PageCache` because the
// Overflow API operates one layer below the public `Chisel` facade.

mod common;

use chisel::overflow::Overflow;
use chisel::page_cache::PageCache;
use chisel::page_io::PageIo;

/// Build a PageCache using the backing indicated by `b`.
///
/// File backing opens a fresh NamedTempFile already held by `b`.
/// Memory backing uses `PageIo::open_in_memory()`, which is non-durable
/// and requires no flock — matching the Chisel `:memory:` design.
fn make_cache(b: &common::Backing) -> PageCache {
    let io = match b {
        common::Backing::File(f) => PageIo::open(f.path(), false).unwrap(),
        common::Backing::Memory => PageIo::open_in_memory().unwrap(),
    };
    PageCache::new(
        io,
        64 * chisel::page::PAGE_SIZE as u64,
        0,
        chisel::DrainInsertion::LruTail,
        chisel::SpillwayLocation::InMemory,
    )
}

fn test_overflow_write_and_read_single_page_body(b: &common::Backing) {
    let mut cache = make_cache(b);
    let value = vec![0xAB; 4000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let read_back = Overflow::read(&mut cache, first_page).unwrap();
    assert_eq!(read_back, value);
}

dual_backing_test!(
    test_overflow_write_and_read_single_page,
    test_overflow_write_and_read_single_page_body
);

fn test_overflow_write_and_read_multi_page_body(b: &common::Backing) {
    let mut cache = make_cache(b);
    let value = vec![0xCD; 20000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let read_back = Overflow::read(&mut cache, first_page).unwrap();
    assert_eq!(read_back, value);
}

dual_backing_test!(
    test_overflow_write_and_read_multi_page,
    test_overflow_write_and_read_multi_page_body
);

fn test_overflow_delete_body(b: &common::Backing) {
    let mut cache = make_cache(b);
    let value = vec![0xEF; 20000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let freed = Overflow::delete(&mut cache, first_page).unwrap();
    assert!(freed.len() >= 3);
}

dual_backing_test!(test_overflow_delete, test_overflow_delete_body);
