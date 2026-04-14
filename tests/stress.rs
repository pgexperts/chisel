mod common;
use common::{open_chisel, Backing};

fn test_many_allocations_body(b: &Backing) {
    let mut db = open_chisel(b);
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

dual_backing_test!(test_many_allocations, test_many_allocations_body);

fn test_many_savepoints_body(b: &Backing) {
    let mut db = open_chisel(b);
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

dual_backing_test!(test_many_savepoints, test_many_savepoints_body);

fn test_multiple_transaction_cycles_body(b: &Backing) {
    let mut db = open_chisel(b);
    for cycle in 0..50 {
        db.begin().unwrap();
        let h = db.allocate(format!("cycle-{cycle}").as_bytes()).unwrap();
        db.commit().unwrap();
        assert_eq!(db.read(h).unwrap(), format!("cycle-{cycle}").as_bytes());
    }
}

dual_backing_test!(
    test_multiple_transaction_cycles,
    test_multiple_transaction_cycles_body
);

fn test_large_values_overflow_body(b: &Backing) {
    let mut db = open_chisel(b);
    db.begin().unwrap();
    let small = db.allocate(b"tiny").unwrap();
    let large = db.allocate(&vec![0xAB; 50_000]).unwrap();
    let medium = db.allocate(&vec![0xCD; 8000]).unwrap();
    db.commit().unwrap();
    assert_eq!(db.read(small).unwrap(), b"tiny");
    assert_eq!(db.read(large).unwrap(), vec![0xAB; 50_000]);
    assert_eq!(db.read(medium).unwrap(), vec![0xCD; 8000]);
}

dual_backing_test!(test_large_values_overflow, test_large_values_overflow_body);
