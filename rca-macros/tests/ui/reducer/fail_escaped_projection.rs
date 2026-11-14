#[rca_macros::reducer(
    state(generated(struct State { value: String })),
    action(supplied(())), body(generated),
)]
struct Feature {}

fn main() {
    let borrowed = {
        let state = State {
            value: String::from("owned"),
        };
        StatePaths::value().get(&state)
    };
    println!("{borrowed}");
}
