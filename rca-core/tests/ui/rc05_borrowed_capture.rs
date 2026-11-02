use rca_core::Task;
fn wrong(value: &usize) {
    let _ = Task::<usize, ()>::run(move |_| async move {
        std::hint::black_box(*value);
    });
}
fn main() {}
