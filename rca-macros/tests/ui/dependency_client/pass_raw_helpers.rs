#![deny(warnings)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use rca_macros::DependencyClient;

#[derive(DependencyClient)]
struct r#Client {
    #[r#dependency(r#member = String::from("member"))]
    value: String,
    #[dependency(r#test = ::r#std::r#sync::r#Arc::new(|value| value))]
    configured: ::r#std::r#sync::r#Arc<
        dyn ::r#core::r#ops::r#Fn(String) -> String
            + ::r#std::r#marker::r#Send
            + ::r#core::r#marker::r#Sync,
    >,
    r#type: r#Arc<dyn r#Fn(usize) -> usize + r#Send + r#Sync>,
}

fn main() {
    let line = line!() + 1;
    let client = r#Client::test_value();
    assert_eq!(client.value, "member");
    assert_eq!(
        (client.configured)(String::from("configured")),
        "configured"
    );

    let panic = catch_unwind(AssertUnwindSafe(|| (client.r#type)(9)))
        .expect_err("an unconfigured endpoint must panic when invoked");
    let message = panic.downcast_ref::<String>().expect("panic text");
    assert!(
        message.contains(&format!("{}.type", std::any::type_name::<Client>())),
        "{message}",
    );
    assert!(!message.contains("r#type"), "{message}");
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
}
