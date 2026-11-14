#[rca_macros::reducer(
    state(empty),
    action(generated(enum Action {
        #[cfg(any())]
        Absent,
        Http(u8),
        HTTP(u8),
    })),
    body(generated),
)]
struct Feature {}

fn main() {}
