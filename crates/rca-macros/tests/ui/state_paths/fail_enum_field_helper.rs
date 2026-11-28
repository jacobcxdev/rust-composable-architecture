use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Child {
        #[rca(skip)]
        child: u8,
    },
}

fn main() {}
