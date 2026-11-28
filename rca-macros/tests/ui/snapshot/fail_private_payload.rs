use rca::Snapshot;
use rca_macros::Snapshot;
struct PrivatePayload;
impl Snapshot for PrivatePayload {
    type Value = Self;
    fn snapshot(&self) -> Self {
        Self
    }
}
#[derive(Snapshot)]
pub struct PublicSource {
    private: PrivatePayload,
}
fn main() {}
