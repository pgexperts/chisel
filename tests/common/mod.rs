// Dual-backing test harness.
//
// Usage:
//   mod common;
//   use common::{Backing, open_chisel};
//
//   fn my_test_body(b: &Backing) {
//       let mut db = open_chisel(b);
//       /* ... assertions ... */
//   }
//
//   common::dual_backing_test!(my_test, my_test_body);
//
// Expands to two #[test] fns: `my_test_file` (NamedTempFile) and
// `my_test_memory` (open_in_memory). The macro keeps each body single-
// source so behavior parity above the I/O layer is continuously verified.

#![allow(dead_code)] // Helpers may be unused in individual test files.

use chisel::{Chisel, Options};
use tempfile::NamedTempFile;

pub enum Backing {
    File(NamedTempFile),
    Memory,
}

impl Backing {
    pub fn fresh_file() -> Backing {
        Backing::File(NamedTempFile::new().expect("tempfile"))
    }
}

pub fn open_chisel(b: &Backing) -> Chisel {
    open_chisel_with(b, Options::default())
}

pub fn open_chisel_with(b: &Backing, opts: Options) -> Chisel {
    match b {
        Backing::File(f) => Chisel::open(f.path(), opts).unwrap(),
        Backing::Memory => Chisel::open_in_memory_with_options(opts).unwrap(),
    }
}

// Requires `mod common;` at the root of the invoking test binary —
// the expansion references `$crate::common::Backing`, and `$crate`
// resolves to the test binary's own root crate.
#[macro_export]
macro_rules! dual_backing_test {
    ($name:ident, $body:ident) => {
        paste::paste! {
            #[test]
            fn [<$name _file>]() {
                let b = $crate::common::Backing::fresh_file();
                $body(&b);
            }

            #[test]
            fn [<$name _memory>]() {
                let b = $crate::common::Backing::Memory;
                $body(&b);
            }
        }
    };
}
