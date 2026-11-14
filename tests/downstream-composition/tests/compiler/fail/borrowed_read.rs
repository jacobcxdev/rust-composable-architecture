// A scoped read cannot return borrowed state. `owned_read.rs` keeps the read
// operation and returns an independent value instead.
pub async fn borrowed(context: architecture::TaskContext<String, ()>) {
    let _ = context.read(|state| state.as_str()).await;
    //~^ ERROR: lifetime may not live long enough
}

fn main() {}
