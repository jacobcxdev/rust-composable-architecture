use rca_core::local::{RemovalSite, Task, TaskContext};

fn escape(
    parent: &TaskContext<Option<u32>, ()>,
    child: &TaskContext<u32, ()>,
    site: &RemovalSite<Option<u32>, u32, ()>,
) {
    let _future = parent.remove_child(
        site,
        child,
        |_| {},
        |_, removed, context| {
            context.task(Task::run(move |_| async move {
                drop(removed);
            }));
        },
    );
}
fn main() {}
