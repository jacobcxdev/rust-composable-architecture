// An empty list must report that at least one reducer is required.

use rca_macros::reducers;

fn main() {
    let _ = reducers![];
}
