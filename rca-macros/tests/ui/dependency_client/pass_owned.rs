use rca_macros::{DependencyClient, DependencyKey};
use std::{future::Future, pin::Pin, sync::Arc};

type Output = Pin<Box<dyn Future<Output = String> + Send>>;

#[derive(DependencyClient)]
struct Client<T, const N: usize>
where
    T: Send + 'static,
{
    #[dependency(member = std::marker::PhantomData)]
    marker: std::marker::PhantomData<T>,
    r#async: ::std::sync::Arc<dyn Fn(T, [u8; N]) -> Output + Send + Sync>,
    #[dependency(test = Arc::new(|| 23))]
    count: std::sync::Arc<dyn std::ops::Fn() -> usize + std::marker::Send + std::marker::Sync>,
}

#[derive(DependencyKey)]
#[dependency(value = Client<String, 2>, live = Client::test_value(), test = Client::test_value())]
struct Key;

fn main() {
    let client = Client::<String, 2> {
        r#async: Arc::new(|name, _| Box::pin(async move { name })),
        ..Client::test_value()
    };
    assert_eq!((client.count)(), 23);
    let _future = (client.r#async)(String::new(), [0; 2]);
}
