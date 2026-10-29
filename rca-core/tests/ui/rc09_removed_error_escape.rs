use rca_core::{Output, RemovalSite, Task, TaskContext};

fn escape(
    parent: &TaskContext<Option<u32>, ()>,
    child: &TaskContext<u32, ()>,
    site: &RemovalSite<Option<u32>, u32, ()>,
    output: Output<u32>,
) {
    let _future = parent.remove_child(
        site,
        child,
        |_| {},
        move |_, removed, context| {
            if let Err(error) = removed.into_output(context, &output, |value| value, |_| {}) {
                context.task(Task::run(move |_| async move {
                    drop(error);
                }));
            }
        },
    );
}
fn main() {}
