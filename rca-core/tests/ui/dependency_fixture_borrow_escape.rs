use rca_core::{DependencyKey, DependencyValues};

struct Number;
impl DependencyKey for Number {
    type Value = usize;
}

fn main() {
    let values = DependencyValues::new();
    let _escaped = values.scope(
        |scope| scope.set::<Number>(1),
        |scope| async move { *scope.get_or_insert::<Number>() },
    );
}
