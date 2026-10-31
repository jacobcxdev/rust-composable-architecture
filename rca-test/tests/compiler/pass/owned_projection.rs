//@ check-pass
use rca_core::{Effect, Reduce};
use rca_test::TestStore;

// The same projection is valid when it returns owned, independent history.
pub fn configure(state: String) {
    let reducer = Reduce::new(|_: &mut String, _: &(), _| Effect::none());
    let _builder = TestStore::projected_builder(state, reducer, |state| state.clone());
}

fn main() {}
