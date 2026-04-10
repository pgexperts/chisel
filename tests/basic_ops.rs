use chisel::page::{self, PAGE_SIZE, PAGE_BODY_SIZE};
use chisel::superblock::Superblock;
use chisel::page::{MAGIC, FORMAT_VERSION, PAGE_ID_NONE};
use chisel::page_io::PageIo;
use chisel::page_cache::PageCache;
use chisel::freemap::FreeMap;
use chisel::data_page::DataPage;
use chisel::handle_table::{HandleTable, HandleEntry, HandleFlags, ENTRIES_PER_LEAF};
use chisel::Chisel;
use tempfile::NamedTempFile;

// --- Page checksum tests ---

#[test]
fn test_checksum_roundtrip() {
    let mut buf = [0u8; PAGE_SIZE];
    buf[0] = 0x42;
    buf[100] = 0xFF;
    page::stamp_checksum(&mut buf);
    assert!(page::verify_checksum(&buf));
}

#[test]
fn test_checksum_detects_corruption() {
    let mut buf = [0u8; PAGE_SIZE];
    buf[0] = 0x42;
    page::stamp_checksum(&mut buf);
    buf[50] = 0xAA;
    assert!(!page::verify_checksum(&buf));
}

#[test]
fn test_checksum_detects_torn_write() {
    let mut buf = [0u8; PAGE_SIZE];
    buf[0] = 0x42;
    page::stamp_checksum(&mut buf);
    buf[PAGE_SIZE - 2] = 0;
    buf[PAGE_SIZE - 1] = 0;
    assert!(!page::verify_checksum(&buf));
}

// --- Superblock tests ---

#[test]
fn test_superblock_roundtrip() {
    let sb = Superblock {
        magic: MAGIC,
        format_version: FORMAT_VERSION,
        txn_counter: 42,
        root_handle_table_page: 5,
        root_freemap_page: 8,
        total_pages: 100,
        next_handle: 50,
        page_size: PAGE_SIZE as u32,
    };
    let buf = sb.serialize();
    let sb2 = Superblock::deserialize(&buf).unwrap();
    assert_eq!(sb, sb2);
}

#[test]
fn test_superblock_checksum_validation() {
    let sb = Superblock {
        magic: MAGIC,
        format_version: FORMAT_VERSION,
        txn_counter: 1,
        root_handle_table_page: PAGE_ID_NONE,
        root_freemap_page: PAGE_ID_NONE,
        total_pages: 2,
        next_handle: 0,
        page_size: PAGE_SIZE as u32,
    };
    let mut buf = sb.serialize();
    buf[10] ^= 0xFF;
    assert!(Superblock::deserialize(&buf).is_none());
}

#[test]
fn test_superblock_selection() {
    let sb1 = Superblock {
        magic: MAGIC, format_version: FORMAT_VERSION, txn_counter: 5,
        root_handle_table_page: 2, root_freemap_page: 3,
        total_pages: 10, next_handle: 3, page_size: PAGE_SIZE as u32,
    };
    let sb2 = Superblock {
        magic: MAGIC, format_version: FORMAT_VERSION, txn_counter: 7,
        root_handle_table_page: 4, root_freemap_page: 5,
        total_pages: 12, next_handle: 5, page_size: PAGE_SIZE as u32,
    };
    let buf1 = sb1.serialize();
    let buf2 = sb2.serialize();
    let selected = Superblock::select(&[buf1, buf2]).unwrap();
    assert_eq!(selected.txn_counter, 7);
}

#[test]
fn test_superblock_selection_with_one_corrupt() {
    let sb1 = Superblock {
        magic: MAGIC, format_version: FORMAT_VERSION, txn_counter: 5,
        root_handle_table_page: 2, root_freemap_page: 3,
        total_pages: 10, next_handle: 3, page_size: PAGE_SIZE as u32,
    };
    let sb2_buf = [0u8; PAGE_SIZE];
    let buf1 = sb1.serialize();
    let selected = Superblock::select(&[buf1, sb2_buf]).unwrap();
    assert_eq!(selected.txn_counter, 5);
}

#[test]
fn test_superblock_selection_both_corrupt() {
    let buf1 = [0u8; PAGE_SIZE];
    let buf2 = [0u8; PAGE_SIZE];
    assert!(Superblock::select(&[buf1, buf2]).is_none());
}

// --- Page I/O tests ---

#[test]
fn test_page_io_write_and_read() {
    let file = NamedTempFile::new().unwrap();
    let mut io = PageIo::open(file.path(), false).unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    buf[0] = 0xAB;
    buf[100] = 0xCD;
    io.write_page(0, &buf).unwrap();
    let read_buf = io.read_page(0).unwrap();
    assert_eq!(read_buf[0], 0xAB);
    assert_eq!(read_buf[100], 0xCD);
}

#[test]
fn test_page_io_multiple_pages() {
    let file = NamedTempFile::new().unwrap();
    let mut io = PageIo::open(file.path(), false).unwrap();
    let mut buf1 = [0u8; PAGE_SIZE];
    let mut buf2 = [0u8; PAGE_SIZE];
    buf1[0] = 1;
    buf2[0] = 2;
    io.write_page(0, &buf1).unwrap();
    io.write_page(1, &buf2).unwrap();
    assert_eq!(io.read_page(0).unwrap()[0], 1);
    assert_eq!(io.read_page(1).unwrap()[0], 2);
}

#[test]
fn test_page_io_fsync() {
    let file = NamedTempFile::new().unwrap();
    let mut io = PageIo::open(file.path(), false).unwrap();
    let buf = [0u8; PAGE_SIZE];
    io.write_page(0, &buf).unwrap();
    io.fsync().unwrap();
}

#[test]
fn test_page_io_file_len() {
    let file = NamedTempFile::new().unwrap();
    let mut io = PageIo::open(file.path(), false).unwrap();
    assert_eq!(io.page_count().unwrap(), 0);
    let buf = [0u8; PAGE_SIZE];
    io.write_page(0, &buf).unwrap();
    assert_eq!(io.page_count().unwrap(), 1);
    io.write_page(2, &buf).unwrap();
    assert_eq!(io.page_count().unwrap(), 3);
}

// --- Page cache tests ---

#[test]
fn test_cache_write_and_read() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 16);

    let page_id = cache.new_page().unwrap();
    {
        let buf = cache.get_mut(page_id).unwrap();
        buf[0] = 0x42;
        buf[100] = 0xFF;
        page::stamp_checksum(buf);
    }
    let buf = cache.get(page_id).unwrap();
    assert_eq!(buf[0], 0x42);
    assert_eq!(buf[100], 0xFF);
}

#[test]
fn test_cache_flush_persists_to_disk() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();

    {
        let io = PageIo::open(&path, false).unwrap();
        let mut cache = PageCache::new(io, 16);
        let page_id = cache.new_page().unwrap();
        {
            let buf = cache.get_mut(page_id).unwrap();
            buf[0] = 0xAB;
            page::stamp_checksum(buf);
        }
        cache.flush().unwrap();
    }

    {
        let mut io = PageIo::open(&path, false).unwrap();
        let buf = io.read_page(0).unwrap();
        assert_eq!(buf[0], 0xAB);
        assert!(page::verify_checksum(&buf));
    }
}

#[test]
fn test_cache_eviction_does_not_evict_dirty() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 2);

    let p0 = cache.new_page().unwrap();
    let p1 = cache.new_page().unwrap();
    {
        let buf = cache.get_mut(p0).unwrap();
        buf[0] = 0x01;
        page::stamp_checksum(buf);
    }
    {
        let buf = cache.get_mut(p1).unwrap();
        buf[0] = 0x02;
        page::stamp_checksum(buf);
    }

    let p2 = cache.new_page().unwrap();
    {
        let buf = cache.get_mut(p2).unwrap();
        buf[0] = 0x03;
        page::stamp_checksum(buf);
    }

    assert_eq!(cache.get(p0).unwrap()[0], 0x01);
    assert_eq!(cache.get(p1).unwrap()[0], 0x02);
    assert_eq!(cache.get(p2).unwrap()[0], 0x03);
}

// --- Freemap tests ---

#[test]
fn test_freemap_allocate_and_free() {
    let mut buf = [0u8; PAGE_SIZE];
    FreeMap::init_page(&mut buf);
    FreeMap::mark_free(&mut buf, 10);
    assert!(FreeMap::is_free(&buf, 10));
    let alloc = FreeMap::allocate_near(&mut buf, 10);
    assert_eq!(alloc, Some(10));
    assert!(!FreeMap::is_free(&buf, 10));
}

#[test]
fn test_freemap_allocate_near_locality() {
    let mut buf = [0u8; PAGE_SIZE];
    FreeMap::init_page(&mut buf);
    FreeMap::mark_free(&mut buf, 100);
    FreeMap::mark_free(&mut buf, 101);
    FreeMap::mark_free(&mut buf, 200);
    let alloc = FreeMap::allocate_near(&mut buf, 99);
    assert_eq!(alloc, Some(100));
}

#[test]
fn test_freemap_allocate_first_free() {
    let mut buf = [0u8; PAGE_SIZE];
    FreeMap::init_page(&mut buf);
    FreeMap::mark_free(&mut buf, 50);
    FreeMap::mark_free(&mut buf, 200);
    let alloc = FreeMap::allocate_first(&mut buf);
    assert_eq!(alloc, Some(50));
    let alloc2 = FreeMap::allocate_first(&mut buf);
    assert_eq!(alloc2, Some(200));
    let alloc3 = FreeMap::allocate_first(&mut buf);
    assert_eq!(alloc3, None);
}

#[test]
fn test_freemap_capacity() {
    assert_eq!(FreeMap::capacity(), PAGE_BODY_SIZE * 8);
}

// --- Data page tests ---

#[test]
fn test_data_page_insert_and_read() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let slot = DataPage::insert(&mut buf, b"hello world").unwrap();
    let data = DataPage::read(&buf, slot).unwrap();
    assert_eq!(data, b"hello world");
}

#[test]
fn test_data_page_multiple_inserts() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let s0 = DataPage::insert(&mut buf, b"aaa").unwrap();
    let s1 = DataPage::insert(&mut buf, b"bbb").unwrap();
    let s2 = DataPage::insert(&mut buf, b"ccc").unwrap();
    assert_eq!(DataPage::read(&buf, s0).unwrap(), b"aaa");
    assert_eq!(DataPage::read(&buf, s1).unwrap(), b"bbb");
    assert_eq!(DataPage::read(&buf, s2).unwrap(), b"ccc");
    assert_eq!(DataPage::slot_count(&buf), 3);
}

#[test]
fn test_data_page_delete_and_compact() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let s0 = DataPage::insert(&mut buf, b"aaa").unwrap();
    let s1 = DataPage::insert(&mut buf, b"bbb").unwrap();
    let _s2 = DataPage::insert(&mut buf, b"ccc").unwrap();
    DataPage::delete(&mut buf, s1);
    assert!(DataPage::read(&buf, s1).is_none());
    let free_before = DataPage::free_space(&buf);
    DataPage::compact(&mut buf);
    let free_after = DataPage::free_space(&buf);
    assert!(free_after > free_before);
    assert_eq!(DataPage::read(&buf, s0).unwrap(), b"aaa");
}

#[test]
fn test_data_page_update_same_size() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let slot = DataPage::insert(&mut buf, b"hello").unwrap();
    DataPage::update(&mut buf, slot, b"world").unwrap();
    assert_eq!(DataPage::read(&buf, slot).unwrap(), b"world");
}

#[test]
fn test_data_page_full() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let big = vec![0xABu8; 2000];
    let mut count = 0;
    while DataPage::insert(&mut buf, &big).is_some() {
        count += 1;
    }
    assert!(count >= 3);
    assert!(count <= 4);
}

#[test]
fn test_data_page_max_value() {
    let mut buf = [0u8; PAGE_SIZE];
    DataPage::init_page(&mut buf);
    let max_val = vec![0xCD; 8162];
    let slot = DataPage::insert(&mut buf, &max_val);
    assert!(slot.is_some());
    assert_eq!(DataPage::read(&buf, slot.unwrap()).unwrap().len(), 8162);
}

// --- Handle table tests ---

#[test]
fn test_handle_table_insert_and_lookup() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let mut ht = HandleTable::new();
    let root = ht.create_root(&mut cache).unwrap();
    let entry = HandleEntry { page_id: 10, slot_index: 3, flags: HandleFlags::Live };
    let new_root = ht.insert(&mut cache, root, 0, &entry).unwrap();
    let found = ht.lookup(&mut cache, new_root, 0).unwrap().unwrap();
    assert_eq!(found.page_id, 10);
    assert_eq!(found.slot_index, 3);
}

#[test]
fn test_handle_table_multiple_entries() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let mut ht = HandleTable::new();
    let mut root = ht.create_root(&mut cache).unwrap();
    for i in 0..10u64 {
        let entry = HandleEntry { page_id: 100 + i, slot_index: i as u16, flags: HandleFlags::Live };
        root = ht.insert(&mut cache, root, i, &entry).unwrap();
    }
    for i in 0..10u64 {
        let found = ht.lookup(&mut cache, root, i).unwrap().unwrap();
        assert_eq!(found.page_id, 100 + i);
        assert_eq!(found.slot_index, i as u16);
    }
}

#[test]
fn test_handle_table_cow_returns_new_root() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let mut ht = HandleTable::new();
    let root1 = ht.create_root(&mut cache).unwrap();
    let entry = HandleEntry { page_id: 10, slot_index: 0, flags: HandleFlags::Live };
    let root2 = ht.insert(&mut cache, root1, 0, &entry).unwrap();
    assert_ne!(root1, root2);
}

#[test]
fn test_handle_table_grows_to_two_levels() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 256);
    let mut ht = HandleTable::new();
    let mut root = ht.create_root(&mut cache).unwrap();
    for i in 0..(ENTRIES_PER_LEAF as u64 + 10) {
        let entry = HandleEntry { page_id: i, slot_index: 0, flags: HandleFlags::Live };
        root = ht.insert(&mut cache, root, i, &entry).unwrap();
    }
    for i in 0..(ENTRIES_PER_LEAF as u64 + 10) {
        let found = ht.lookup(&mut cache, root, i).unwrap().unwrap();
        assert_eq!(found.page_id, i);
    }
}

#[test]
fn test_handle_table_delete() {
    let file = NamedTempFile::new().unwrap();
    let io = PageIo::open(file.path(), false).unwrap();
    let mut cache = PageCache::new(io, 64);
    let mut ht = HandleTable::new();
    let mut root = ht.create_root(&mut cache).unwrap();
    let entry = HandleEntry { page_id: 10, slot_index: 0, flags: HandleFlags::Live };
    root = ht.insert(&mut cache, root, 0, &entry).unwrap();
    root = ht.delete(&mut cache, root, 0).unwrap();
    let found = ht.lookup(&mut cache, root, 0).unwrap();
    assert!(found.is_none());
}

// --- Chisel public API tests ---

#[test]
fn test_chisel_public_api_roundtrip() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let mut db = Chisel::open(&path, Default::default()).unwrap();
    db.begin().unwrap();
    let h1 = db.allocate(b"value one").unwrap();
    let h2 = db.allocate(b"value two").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"value one");
    assert_eq!(db.read(h2).unwrap(), b"value two");
    db.begin().unwrap();
    db.update(h1, b"updated").unwrap();
    db.delete(h2).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(h1).unwrap(), b"updated");
    assert!(db.read(h2).is_err());
    db.close().unwrap();
}

#[test]
fn test_chisel_reopen() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    let handle;
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        db.begin().unwrap();
        handle = db.allocate(b"survive reopen").unwrap();
        db.commit().unwrap();
        db.close().unwrap();
    }
    {
        let mut db = Chisel::open(&path, Default::default()).unwrap();
        assert_eq!(db.read(handle).unwrap(), b"survive reopen");
        db.close().unwrap();
    }
}

#[test]
fn test_chisel_stats() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    let stats = db.stats().unwrap();
    assert_eq!(stats.handle_count, 0);
    db.begin().unwrap();
    db.allocate(b"one").unwrap();
    db.allocate(b"two").unwrap();
    db.commit().unwrap();
    let stats = db.stats().unwrap();
    assert_eq!(stats.handle_count, 2);
}
