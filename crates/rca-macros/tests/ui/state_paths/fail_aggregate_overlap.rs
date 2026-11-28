use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Pair(String, usize),
}

fn main() {
    let path = StatePaths::pair();
    let mut root = State::Pair(String::new(), 0);
    let first = path.get_mut(&mut root).unwrap();
    let second = path.get_mut(&mut root).unwrap();
    first.0.push_str("first");
    second.0.push_str("second");
}
