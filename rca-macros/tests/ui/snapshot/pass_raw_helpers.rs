#![deny(warnings)]

use rca::Snapshot;
use rca_macros::Snapshot;
use std::cell::Cell;

fn capture(value: &&str) -> String {
    value.to_uppercase()
}

#[derive(Snapshot)]
#[r#doc = "A source with raw helper and built-in attribute spellings."]
#[r#cfg(all())]
#[r#snapshot(r#name = History)]
struct Source<'a> {
    #[r#snapshot(r#with = capture, r#value = String)]
    #[snapshot(r#redact)]
    secret: &'a str,
    #[r#snapshot(r#omit = "Bookkeeping does not affect captured state")]
    bookkeeping: Cell<u8>,
    #[r#doc = "A captured counter."]
    count: usize,
    #[r#cfg(any())]
    absent: MissingType,
}

struct Destination {
    value: String,
}

#[derive(Snapshot)]
#[r#snapshot(r#into = Destination)]
struct IntoSource<'a> {
    #[r#snapshot(r#with = capture, r#value = String)]
    value: &'a str,
}

fn main() {
    let original = String::from("secret");
    let changed = String::from("changed");
    let mut source = Source {
        secret: &original,
        bookkeeping: Cell::new(1),
        count: 3,
    };
    let before = source.snapshot();
    assert_eq!(
        before,
        History {
            secret: String::from("SECRET"),
            count: 3,
        }
    );

    source.bookkeeping.set(2);
    assert_eq!(source.bookkeeping.get(), 2);
    assert_eq!(before, source.snapshot());
    assert_eq!(
        format!("{before:?}"),
        "History { secret: \"<redacted>\", count: 3 }"
    );

    source.secret = &changed;
    let after = source.snapshot();
    assert_eq!(before.secret, "SECRET");
    assert_eq!(after.secret, "CHANGED");
    assert_ne!(before, after);
    assert_eq!(format!("{before:?}"), format!("{after:?}"));

    let destination: Destination = IntoSource { value: &original }.snapshot();
    assert_eq!(destination.value, "SECRET");
}
