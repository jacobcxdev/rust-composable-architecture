use rca_core::Shared;

fn main() {
    let shared = Shared::new(Some(0));
    let _future = shared
        .update(
            |value| async move {
                std::future::pending::<()>().await;
                *value = Some(1);
            },
            || {},
        )
        .unwrap();
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let _future = projection
        .update(
            |value| async move {
                std::future::pending::<()>().await;
                *value = 1;
            },
            || {},
        )
        .unwrap();
}
