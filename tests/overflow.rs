use chisel::overflow::Overflow;
use chisel::page_cache::PageCache;
use chisel::page_io::PageIo;
use tempfile::NamedTempFile;

#[test]
fn test_overflow_write_and_read_single_page() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let value = vec![0xAB; 4000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let read_back = Overflow::read(&mut cache, first_page).unwrap();
    assert_eq!(read_back, value);
}

#[test]
fn test_overflow_write_and_read_multi_page() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let value = vec![0xCD; 20000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let read_back = Overflow::read(&mut cache, first_page).unwrap();
    assert_eq!(read_back, value);
}

#[test]
fn test_overflow_delete() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let value = vec![0xEF; 20000];
    let first_page = Overflow::write(&mut cache, &value).unwrap();
    let freed = Overflow::delete(&mut cache, first_page).unwrap();
    assert!(freed.len() >= 3);
}
