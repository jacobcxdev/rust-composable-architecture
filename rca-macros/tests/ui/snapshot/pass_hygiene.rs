#![deny(warnings)]
#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    reason = "collision names and unused source fields deliberately exercise derive hygiene"
)]
// Owned custom projections and omitted authority are valid alternatives to
// the rejected captures in fail_capture_types.rs.
use rca::Snapshot;
use rca_macros::Snapshot;
use std::{marker::PhantomData, rc::Rc};

const __rca_snapshot_field0: usize = 7;
const __rca_snapshot_formatter: usize = 8;
const __rca_snapshot_debug: usize = 9;

#[derive(Snapshot)]
struct r#type<__RcaSnapshotField0, const __rca_snapshot_field1: usize> {
    #[cfg_attr(all(), doc = "Configured captured field.")]
    r#async: __RcaSnapshotField0,
    bytes: [u8; __rca_snapshot_field1],
    #[cfg(any())]
    gone: Missing,
}

#[derive(Snapshot)]
enum État {
    r#type {
        r#async: usize,
    },
    #[cfg_attr(all(), cfg(any()))]
    Absent(Missing),
}

#[derive(Snapshot)]
#[snapshot(name = History)]
struct Omitted<'a, T, const N: usize>(
    #[snapshot(omit = "Lifetime/type/const marker carries no compared value")]
    PhantomData<(&'a T, [Rc<()>; N])>,
);

struct NoTraits;
mod other {
    pub struct Source;
    impl rca::Snapshot for Source {
        type Value = usize;
        fn snapshot(&self) -> usize {
            42
        }
    }
}
#[derive(Snapshot)]
struct Source {
    nested: other::Source,
}

fn capture<T>(_: &T) -> NoTraits {
    NoTraits
}
#[derive(Snapshot)]
struct Opaque<T>(#[snapshot(with = capture::<T>, value = NoTraits)] T);

fn owned(value: &&str) -> String {
    value.to_string()
}
struct Destination<T> {
    value: T,
}
#[derive(Snapshot)]
#[snapshot(into = Destination<String>)]
struct DestinationSource<'a> {
    #[snapshot(with = owned, value = String)]
    value: &'a str,
}

fn has_next(value: &Option<Box<Recursive>>) -> bool {
    value.is_some()
}
#[derive(Snapshot)]
struct Recursive {
    #[snapshot(with = has_next, value = bool)]
    next: Option<Box<Self>>,
}

struct PrivatePayload(usize);
fn public_value(value: &PrivatePayload) -> usize {
    value.0
}
#[derive(Snapshot)]
pub struct PublicProjection {
    #[snapshot(with = public_value, value = usize)]
    private: PrivatePayload,
}

fn main() {
    assert_eq!(
        Source {
            nested: other::Source
        }
        .snapshot()
        .nested,
        42
    );
    let value = r#type {
        r#async: 3usize,
        bytes: [5, 7],
    }
    .snapshot();
    assert_eq!(
        value,
        typeSnapshot {
            r#async: 3,
            bytes: [5, 7]
        }
    );
    let value = État::r#type { r#async: 4 }.snapshot();
    assert_eq!(value, ÉtatSnapshot::r#type { r#async: 4 });
    assert!(format!("{value:?}").contains("async"));
    let history: History = Omitted::<'_, Rc<()>, 9>(PhantomData).snapshot();
    fn transfer<T: Send + Sync + 'static>(_: T) {}
    transfer(history);
    let _ = Opaque(Rc::new(())).snapshot();
    assert_eq!(
        DestinationSource { value: "owned" }.snapshot().value,
        "owned"
    );
    assert!(
        Recursive {
            next: Some(Box::new(Recursive { next: None }))
        }
        .snapshot()
        .next
    );
    assert_eq!(
        PublicProjection {
            private: PrivatePayload(5)
        }
        .snapshot()
        .private,
        5
    );
}
