use rca_macros::Snapshot;

#[derive(Snapshot)]
struct r#Source {
    next: Box<r#Source>,
}

#[derive(Snapshot)]
struct r#Mixed {
    next: Box<Mixed>,
}

fn main() {}
