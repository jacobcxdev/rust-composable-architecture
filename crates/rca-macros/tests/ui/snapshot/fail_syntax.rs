use rca_macros::Snapshot;
#[derive(Snapshot)]
#[snapshot(name = "NotAnIdentifier")]
struct StringName;
#[derive(Snapshot)]
#[snapshot(into = <Source as Trait>::Value)]
struct Qualified {
    value: usize,
}
#[derive(Snapshot)]
struct Closure(#[snapshot(with = |x| x, value = usize)] usize);
#[derive(Snapshot)]
struct BadType(#[snapshot(with = capture, value = *usize)] usize);
#[derive(Snapshot)]
struct DuplicateAcrossAttributes {
    #[snapshot(redact)]
    #[snapshot(redact)]
    value: usize,
}
#[derive(Snapshot)]
#[snapshot(mystery)]
struct UnknownItem;

macro_rules! malformed_snapshot {
    () => {
        #[derive(Snapshot)]
        struct $crate {
            value: usize,
        }
    };
}

malformed_snapshot!();

fn main() {}
