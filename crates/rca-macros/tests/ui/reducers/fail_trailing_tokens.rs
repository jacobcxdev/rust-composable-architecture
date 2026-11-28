// Adjacent reducers must report the missing comma.

use rca_macros::reducers;

fn main() {
    let _ = reducers![First Second];
}
