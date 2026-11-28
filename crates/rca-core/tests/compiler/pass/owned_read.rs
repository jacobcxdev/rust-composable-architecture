//@ check-pass
// Positive counterpart to `borrowed_read.rs`: state ownership stays in Store.
pub async fn owned(context: rca_core::TaskContext<String, ()>) {
    let _: String = context.read(Clone::clone).await.unwrap();
}

fn main() {}
