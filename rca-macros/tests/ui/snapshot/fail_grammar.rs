use rca_macros::Snapshot;

#[derive(Snapshot)]
#[snapshot(name = One, into = Output)]
struct Conflict {
    field: usize,
}

#[derive(Snapshot)]
#[snapshot(name = First, name = Second)]
struct DuplicateItem;

#[derive(Snapshot)]
#[snapshot(redact)]
struct FieldOptionOnItem;

#[derive(Snapshot)]
struct InvalidFields {
    #[snapshot(omit)]
    bare: usize,
    #[snapshot(omit = "   ")]
    empty: usize,
    #[snapshot(omit = "reason", redact)]
    conflict: usize,
    #[snapshot(with = capture)]
    missing_value: usize,
    #[snapshot(value = usize)]
    missing_function: usize,
    #[snapshot(redact, redact)]
    duplicate: usize,
    #[snapshot(skip)]
    unknown: usize,
    #[snapshot(name = Field)]
    item_option: usize,
}

#[derive(Snapshot)]
enum BadVariant {
    #[snapshot(omit = "case")]
    Value,
}

#[derive(Snapshot)]
union BadUnion {
    value: usize,
}

#[derive(Snapshot)]
struct Recursive {
    next: Option<Box<Self>>,
}

fn main() {}
