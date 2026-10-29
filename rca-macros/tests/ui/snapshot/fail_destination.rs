use rca_macros::Snapshot;
struct Output {
    required: usize,
}

#[derive(Snapshot)]
#[snapshot(into = Output)]
struct Missing {}
#[derive(Snapshot)]
#[snapshot(into = Output)]
struct Extra {
    required: usize,
    extra: usize,
}
#[derive(Snapshot)]
#[snapshot(into = Output)]
struct WrongType {
    required: String,
}
#[derive(Snapshot)]
#[snapshot(into = Output)]
struct Redacted {
    #[snapshot(redact)]
    required: usize,
}
#[derive(Snapshot)]
#[snapshot(into = Output)]
struct Tuple(usize);
#[derive(Snapshot)]
#[snapshot(into = Output)]
enum Enum {
    Value,
}

#[derive(Snapshot)]
struct Collision;
struct CollisionSnapshot;

fn main() {}
