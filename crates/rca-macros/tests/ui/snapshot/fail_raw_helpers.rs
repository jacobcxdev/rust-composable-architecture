use rca_macros::Snapshot;

#[derive(Snapshot)]
#[snapshot(name = First)]
#[r#snapshot(r#name)]
struct DuplicateItem;

#[derive(Snapshot)]
#[r#snapshot(r#redact)]
struct FieldOptionOnItem;

#[derive(Snapshot)]
#[r#snapshot(r#nested::r#name = Other)]
struct QualifiedItem;

#[derive(Snapshot)]
struct InvalidFields {
    #[snapshot(redact)]
    #[r#snapshot(r#redact)]
    duplicate: usize,
    #[r#snapshot(r#with = capture, with)]
    duplicate_before_missing_value: usize,
    #[r#snapshot(r#name = Field)]
    item_option: usize,
    #[r#snapshot(r#with = capture, r#nested::r#value = usize)]
    qualified_before_missing_value: usize,
}

#[derive(Snapshot)]
enum BadVariant {
    #[r#snapshot(r#omit = "Case identity cannot be omitted")]
    Value,
}

fn main() {}
