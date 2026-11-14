use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Duplicate {
    #[rca(keyed, r#keyed)]
    Child(u8),
}

#[derive(ActionPaths)]
#[r#rca(skip)]
enum Root {
    Child,
}

#[derive(ActionPaths)]
enum Field {
    #[rca(skip)]
    Child {
        #[r#rca(keyed)]
        value: u8,
    },
}

fn main() {}
