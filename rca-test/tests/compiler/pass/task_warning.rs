//@ error-in-other-file: 1 warning emitted
//@ normalize-stderr-test: "rca_test::TestActionTask" -> "TestActionTask"
// rustc qualifies this name only when the Local sibling is also available.
// A test task is causal work, not cancellation-on-drop authority.
// Unwrap the Result so this checks TestActionTask's warning, not Result's.
pub async fn ignored(store: &mut rca_test::TestStore<String, ()>) {
    store.send(()).await.unwrap();
    //~^ WARN: /unused `(rca_test::)?TestActionTask` that must be used/
}

fn main() {}
