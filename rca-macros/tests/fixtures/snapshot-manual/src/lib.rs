//! Exercises owned capture without the facade's macro or runtime features.

use rca::Snapshot;

/// Borrows source text while its snapshot owns an independent string.
pub struct Borrowed<'a>(pub &'a str);
impl Snapshot for Borrowed<'_> {
    type Value = String;
    fn snapshot(&self) -> String {
        self.0.into()
    }
}

#[test]
fn manual_snapshot_is_owned_without_macros() {
    let value = {
        let source = String::from("history");
        Borrowed(&source).snapshot()
    };
    assert_eq!(value, "history");
}
