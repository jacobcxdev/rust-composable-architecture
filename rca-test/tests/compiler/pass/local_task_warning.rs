//@ error-in-other-file: 1 warning emitted
// Local test tasks retain the same warning, without imposing Send on their state.
// Unwrap the Result so this checks TestActionTask's warning, not Result's.
pub async fn ignored(store: &mut rca_test::local::TestStore<std::rc::Rc<String>, ()>) {
    store.send(()).await.unwrap();
    //~^ WARN: unused `rca_test::local::TestActionTask` that must be used
}

fn main() {}
