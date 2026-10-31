#[rca_macros::reducer(
    state(generated(struct State { #[reducer(case = alert)] alert: Option<usize> })),
    action(generated(enum Action { #[reducer(case = alert)] Alert(_) })),
    body(supplied(body)),
    case(alert, policy = ephemeral, action = empty),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
