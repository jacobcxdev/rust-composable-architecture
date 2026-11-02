use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Pair(String, usize),
}

fn main() {
    let borrowed;
    {
        let mut root = State::Pair(String::from("local"), 1);
        borrowed = StatePaths::pair().get_mut(&mut root).unwrap();
    }
    borrowed.0.push_str("escaped");
}
