use std::sync::Arc;

use rca_macros::DependencyClient;

#[derive(DependencyClient)]
struct Client<'a> {
    #[dependency(test = Arc::new(|text| text))]
    call: Arc<dyn Fn(&'a str) -> &'a str + Send + Sync + 'a>,
}

fn escaped_result() -> &'static str {
    let text = String::from("borrowed");
    let client = Client::test_value();
    (client.call)(&text)
}

fn escaped_capture<'a>(text: &'a str) -> Arc<dyn Fn() -> &'static str + Send + Sync> {
    Arc::new(move || text)
}

fn main() {}
