#![deny(warnings)]

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use rca_macros::DependencyClient;

#[derive(DependencyClient)]
struct Client<'env, T> {
    borrowed: Arc<dyn Fn(&str) -> usize + Send + Sync>,
    higher_ranked: Arc<dyn for<'a> Fn(&'a str) -> &'a str + Send + Sync>,
    static_borrow: Arc<dyn Fn(&'static str) -> &'static str + Send + Sync>,
    borrowed_output: Arc<dyn Fn() -> &'static str + Send + Sync>,
    nested: Arc<dyn Fn(Vec<&'static str>) -> usize + Send + Sync>,
    pointers: Arc<dyn Fn(Vec<&'static for<'a> fn(&'a str)>) + Send + Sync>,
    generic: Arc<dyn Fn(&'env T) -> &'env T + Send + Sync + 'env>,
    container: Arc<dyn Fn(Cow<'env, str>) -> Cow<'env, str> + Send + Sync + 'env>,
    mutable: Arc<dyn Fn(&mut usize) + Send + Sync>,
}

#[derive(DependencyClient)]
struct Configured<'env, T> {
    #[dependency(test = Arc::new(|text| text))]
    higher_ranked: Arc<dyn for<'a> Fn(&'a str) -> &'a str + Send + Sync>,
    #[dependency(test = Arc::new(|value| value))]
    generic: Arc<dyn Fn(&'env T) -> &'env T + Send + Sync + 'env>,
}

fn main() {
    let text = String::from("borrowed");
    let local = std::rc::Rc::new(String::from("local"));
    let mut count = 1;
    let configured = Client {
        borrowed: Arc::new(str::len),
        higher_ranked: Arc::new(|text| text),
        static_borrow: Arc::new(|text| text),
        borrowed_output: Arc::new(|| "static"),
        nested: Arc::new(|values| values.len()),
        pointers: Arc::new(|callbacks| callbacks[0]("static")),
        generic: Arc::new(|value| value),
        container: Arc::new(|text| text),
        mutable: Arc::new(|value| *value += 1),
    };
    assert_eq!((configured.borrowed)(&text), text.len());
    assert!(std::ptr::eq(
        (configured.higher_ranked)(&text),
        text.as_str()
    ));
    assert_eq!((configured.static_borrow)("static"), "static");
    assert_eq!((configured.borrowed_output)(), "static");
    assert_eq!((configured.nested)(vec!["nested"]), 1);
    static CALLBACK: for<'a> fn(&'a str) = |_| {};
    (configured.pointers)(vec![&CALLBACK]);
    assert!(std::ptr::eq((configured.generic)(&local), &local));
    assert!(matches!(
        (configured.container)(Cow::Borrowed(&text)),
        Cow::Borrowed(_)
    ));
    (configured.mutable)(&mut count);
    assert_eq!(count, 2);

    let explicit = Configured::test_value();
    assert!(std::ptr::eq((explicit.higher_ranked)(&text), text.as_str()));
    assert!(std::ptr::eq((explicit.generic)(&local), &local));

    let line = line!() + 1;
    let unconfigured = Client::test_value();
    let panic = catch_unwind(AssertUnwindSafe(|| (unconfigured.borrowed)(&text)))
        .expect_err("unconfigured borrowed endpoint");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains(".borrowed"), "{message}");
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.higher_ranked)(&text))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.static_borrow)("static"))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.borrowed_output)())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.nested)(vec!["nested"]))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| (unconfigured.pointers)(vec![
            &CALLBACK
        ])))
        .is_err()
    );
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.generic)(&local))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| (unconfigured.container)(
            Cow::Borrowed(&text)
        )))
        .is_err()
    );
    assert!(catch_unwind(AssertUnwindSafe(|| (unconfigured.mutable)(&mut count))).is_err());
    assert_eq!(count, 2);
}
