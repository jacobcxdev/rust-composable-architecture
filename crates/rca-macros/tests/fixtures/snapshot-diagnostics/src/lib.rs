//! Exercises manual capture with diagnostics enabled and derive macros disabled.

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

#[test]
fn diagnostics_uses_the_manual_capture() {
    let mut source = Borrowed("before");
    let mut observed = false;
    let result: Result<(), ()> = rca::diagnostics::checkpoint(
        &mut source,
        "update",
        Snapshot::snapshot,
        |source| {
            source.0 = "after";
            Ok(())
        },
        |checkpoint| {
            assert!(checkpoint.is_changed());
            observed = true;
        },
    );
    assert_eq!(result, Ok(()));
    assert!(observed);
}
