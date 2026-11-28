use rca_core::Shared;

fn main() {
    let shared = Shared::new(0);
    let _future = shared
        .try_update(
            |value| {
                Ok::<_, ()>(async move {
                    std::future::pending::<()>().await;
                    *value = 1;
                })
            },
            || {},
        )
        .unwrap();
    let _read = shared
        .read(|value| async move {
            std::future::pending::<()>().await;
            *value
        })
        .unwrap();
}
