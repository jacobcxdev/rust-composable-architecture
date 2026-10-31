use rca_macros::DependencyClient;
use std::{borrow::Cow, sync::Arc};

#[derive(DependencyClient)]
struct LifetimeClient {
    #[dependency(test = Arc::new(|text| assert_eq!(text, "owned")))]
    endpoint: Arc<dyn Fn(Cow<'static, str>) + Send + Sync>,
}

#[derive(DependencyClient)]
struct BinderClient {
    #[dependency(test = Arc::new(|callback| callback()))]
    endpoint: Arc<dyn Fn(fn()) + Send + Sync>,
}

#[derive(DependencyClient)]
struct ArrayClient {
    #[dependency(test = Arc::new(|bytes| bytes.len()))]
    endpoint: Arc<
        dyn Fn(
                [u8; {
                    let length: &usize = &3;
                    *length
                }],
            ) -> usize
            + Send
            + Sync,
    >,
}

#[derive(DependencyClient)]
struct CallbackClient {
    #[dependency(test = Arc::new(|callback, text| callback(&text).to_owned()))]
    configured: Arc<dyn Fn(for<'a> fn(&'a str) -> &'a str, String) -> String + Send + Sync>,
    unconfigured: Arc<dyn Fn(for<'a> fn(&'a str) -> &'a str) -> usize + Send + Sync>,
    #[dependency(test = Arc::new(|_callback| 7))]
    external: Arc<dyn Fn(unsafe extern "C" fn(*const u8) -> usize) -> usize + Send + Sync>,
}

fn identity(text: &str) -> &str {
    text
}

unsafe extern "C" fn external(_input: *const u8) -> usize {
    7
}

fn callback() {}

fn main() {
    let lifetime = LifetimeClient::test_value();
    (lifetime.endpoint)(Cow::Borrowed("owned"));
    let binder = BinderClient::test_value();
    (binder.endpoint)(callback);
    assert_eq!((ArrayClient::test_value().endpoint)([1, 2, 3]), 3);
    let client = CallbackClient::test_value();
    assert_eq!(
        (client.configured)(identity, String::from("owned")),
        "owned"
    );
    assert_eq!((client.external)(external), 7);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (client.unconfigured)(identity)
        }))
        .is_err()
    );
}
