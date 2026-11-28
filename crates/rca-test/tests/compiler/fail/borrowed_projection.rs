use rca_core::{Effect, Reduce};
use rca_test::TestStore;

// Historical snapshots must not borrow the mutable state being tested.
pub fn configure(state: String) {
    let reducer = Reduce::new(|_: &mut String, _: &(), _| Effect::none());
    let _builder = TestStore::projected_builder(state, reducer, |state| state.as_str());
    //~^ ERROR: lifetime may not live long enough
}

fn main() {}
