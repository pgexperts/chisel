use chisel::Chisel;
use tempfile::NamedTempFile;

#[test]
fn test_many_allocations() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    db.begin().unwrap();
    let mut handles = Vec::new();
    for i in 0..1000u64 {
        let value = format!("value-{i}");
        handles.push(db.allocate(value.as_bytes()).unwrap());
    }
    db.commit().unwrap();
    for (i, &h) in handles.iter().enumerate() {
        let expected = format!("value-{i}");
        assert_eq!(db.read(h).unwrap(), expected.as_bytes());
    }
}

#[test]
fn test_many_savepoints() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    db.begin().unwrap();
    let base = db.allocate(b"base").unwrap();
    for i in 0..20 {
        db.savepoint(&format!("sp-{i}")).unwrap();
        db.allocate(&format!("sp-{i}-value").into_bytes()).unwrap();
    }
    db.rollback_to("sp-0").unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(base).unwrap(), b"base");
}

#[test]
fn test_multiple_transaction_cycles() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    for cycle in 0..50 {
        db.begin().unwrap();
        let h = db.allocate(format!("cycle-{cycle}").as_bytes()).unwrap();
        db.commit().unwrap();
        assert_eq!(db.read(h).unwrap(), format!("cycle-{cycle}").as_bytes());
    }
}

#[test]
fn test_large_values_overflow() {
    let file = NamedTempFile::new().unwrap();
    let mut db = Chisel::open(file.path(), Default::default()).unwrap();
    db.begin().unwrap();
    let small = db.allocate(b"tiny").unwrap();
    let large = db.allocate(&vec![0xAB; 50_000]).unwrap();
    let medium = db.allocate(&vec![0xCD; 8000]).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(small).unwrap(), b"tiny");
    assert_eq!(db.read(large).unwrap(), vec![0xAB; 50_000]);
    assert_eq!(db.read(medium).unwrap(), vec![0xCD; 8000]);
}
