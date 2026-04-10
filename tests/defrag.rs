use chisel::Chisel;
use tempfile::NamedTempFile;

#[test]
fn test_defrag_reclaims_space_after_deletes() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();

    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..50 {
        handles.push(db.allocate(&vec![i as u8; 200]).unwrap());
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for &h in &handles[5..] {
        db.delete(h).unwrap();
    }
    db.commit().unwrap();

    db.begin().unwrap();
    let result = db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    for &h in &handles[..5] {
        assert!(db.read(h).is_ok());
    }
    assert!(result.pages_freed > 0);
}

#[test]
fn test_defrag_preserves_all_data() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();

    db.begin().unwrap();
    let h1 = db.allocate(b"alpha").unwrap();
    let h2 = db.allocate(b"beta").unwrap();
    let h3 = db.allocate(b"gamma").unwrap();
    db.commit().unwrap();

    db.begin().unwrap();
    db.defrag(Default::default()).unwrap();
    db.commit().unwrap();

    assert_eq!(db.read(h1).unwrap(), b"alpha");
    assert_eq!(db.read(h2).unwrap(), b"beta");
    assert_eq!(db.read(h3).unwrap(), b"gamma");
}
