// Two commas must report a missing reducer expression.

use rca_macros::reducers;

fn main() {
    let _ = reducers![First,, Second];
}
