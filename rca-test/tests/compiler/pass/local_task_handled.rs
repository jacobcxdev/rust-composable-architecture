//@ check-pass
// Local counterpart: finish and explicit discard remain valid with non-Send state.
pub async fn waited(store: &mut rca_test::local::TestStore<std::rc::Rc<String>, ()>) {
    store.send(()).await.unwrap().finish().await.unwrap();
}

pub async fn detached(store: &mut rca_test::local::TestStore<std::rc::Rc<String>, ()>) {
    drop(store.send(()).await.unwrap());
}

fn main() {}
