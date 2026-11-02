use rca_macros::StatePaths;

#[derive(StatePaths)]
struct Duplicate {
    #[rca(optional)]
    #[r#rca(r#optional)]
    value: Option<u8>,
}

#[derive(StatePaths)]
enum SkippedTuple {
    #[r#rca(r#skip)]
    Hidden(#[r#rca(r#required)] u8),
}

#[derive(StatePaths)]
enum SkippedNamed {
    #[rca(skip)]
    Hidden {
        #[r#rca(r#optional)]
        value: Option<u8>,
    },
}

#[derive(StatePaths)]
#[r#rca(r#skip)]
struct InvalidRoot {
    value: u8,
}

fn main() {}
