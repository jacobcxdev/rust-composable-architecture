//! Exercises a manual capture as a TestStore projection without derive macros.

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

/// Supplies an inert reducer so the consumer isolates projection compatibility.
pub struct Feature;
impl rca::Reducer for Feature {
    type State = Borrowed<'static>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Self::State,
        _: &(),
        _: &mut rca::ReduceContext<'_>,
    ) -> rca::Effect<()> {
        rca::Effect::none()
    }
}

#[test]
fn hosted_projection_accepts_the_manual_capture() {
    let store = rca::TestStore::projected(Borrowed("owned"), Feature, Snapshot::snapshot);
    drop(store);
}
