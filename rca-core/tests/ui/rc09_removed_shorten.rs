use rca_core::RemovedChild;

fn shorten<'long: 'short, 'short>(removed: RemovedChild<'long, u32>) -> RemovedChild<'short, u32> {
    removed
}
fn main() {}
