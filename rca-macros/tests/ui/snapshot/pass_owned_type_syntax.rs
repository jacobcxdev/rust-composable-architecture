#![deny(warnings)]

use rca::Snapshot as _;
use rca_macros::Snapshot;

fn capture_bytes(value: &u8) -> [u8; 3] {
    [*value; 3]
}

fn identity(value: &str) -> &str {
    value
}

fn capture_pointer(_: &u8) -> for<'a> fn(&'a str) -> &'a str {
    identity
}

#[derive(Snapshot)]
struct Custom {
    #[snapshot(with = capture_bytes, value = [u8; {
        let length: &usize = &3;
        *length
    }])]
    bytes: u8,
    #[snapshot(with = capture_pointer, value = for<'a> fn(&'a str) -> &'a str)]
    pointer: u8,
}

#[derive(Snapshot)]
struct Constants {
    bytes: [u8; {
        type Constants = u8;
        std::mem::size_of::<Constants>()
    }],
}

fn main() {
    let history = {
        let mut source = Custom {
            bytes: 7,
            pointer: 0,
        };
        let history = source.snapshot();
        source.bytes = 9;
        assert_eq!(source.snapshot().bytes, [9; 3]);
        history
    };
    assert_eq!(history.bytes, [7; 3]);
    fn require_owned<T: Send + Sync + 'static>(_: &T) {}
    require_owned(&history);
    let local = String::from("borrowed argument, not a captured borrow");
    assert_eq!((history.pointer)(&local), local);

    let source = Constants { bytes: [3] };
    assert_eq!(source.snapshot().bytes, [3]);
}
