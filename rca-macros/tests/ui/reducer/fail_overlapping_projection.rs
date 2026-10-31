#[rca_macros::reducer(
    state(generated(struct State { value: String })),
    action(supplied(())), body(generated),
)]
struct Feature {}

fn main() {
    let mut state = State {
        value: String::from("owned"),
    };
    let path = StatePaths::value();
    let first = path.get_mut(&mut state);
    let second = path.get_mut(&mut state);
    first.push_str(second);
}
