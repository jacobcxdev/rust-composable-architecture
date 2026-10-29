use rca_macros::DependencyClient;
use std::sync::Arc;

#[derive(DependencyClient)]
struct Client<const N: usize> {
    #[cfg(any())]
    disabled: MissingType,
    #[dependency(member = [3; N])]
    bytes: [u8; N],
    #[dependency(test = Arc::new(|value| value))]
    r#type: Arc<dyn Fn(String) -> String + Send + Sync>,
}

fn main() {
    let client = Client::<2>::test_value();
    assert_eq!(client.bytes, [3, 3]);
    assert_eq!((client.r#type)(String::from("owned")), "owned");
}
