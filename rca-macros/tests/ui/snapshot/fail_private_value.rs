use rca::Snapshot;
use rca_macros::Snapshot;
mod private_field {
    use super::*;
    #[derive(Snapshot)]
    pub struct Source {
        field: usize,
    }
    pub fn captured() -> <Source as Snapshot>::Value {
        Source { field: 1 }.snapshot()
    }
}

fn main() {
    let _ = private_field::captured().field;
}
