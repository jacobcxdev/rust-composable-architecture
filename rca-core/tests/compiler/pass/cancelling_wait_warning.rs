//@ error-in-other-file: 2 warnings emitted
// Unlike an ordinary task drop, dropping this unpolled wait requests cancellation.
// `cancelling_wait_handled.rs` shows awaiting and explicit cancellation by drop.
pub async fn ignored(store: &rca_core::Store<String, ()>) {
    let task = store.send(()).await.unwrap();
    task.finish_cancelling_on_drop();
    //~^ WARN: unused implementer of `Future` that must be used
    //~| WARN: unused return value of `ActionTask::finish_cancelling_on_drop` that must be used
}

fn main() {}
