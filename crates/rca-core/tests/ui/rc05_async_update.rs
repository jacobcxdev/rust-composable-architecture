use rca_core::TaskContext;
fn wrong(context: TaskContext<usize, ()>) {
    let _ = context.update(|state| async move {
        *state += 1;
    });
}
fn main() {}
