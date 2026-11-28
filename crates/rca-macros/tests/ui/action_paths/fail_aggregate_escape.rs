use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    Pair(String, usize),
}

fn main() {
    let borrowed;
    {
        let root = Action::Pair(String::from("local"), 1);
        borrowed = ActionPaths::pair().extract(&root).unwrap();
    }
    let _ = borrowed.0.len();
}
