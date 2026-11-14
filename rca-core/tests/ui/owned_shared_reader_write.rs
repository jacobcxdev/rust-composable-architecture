use rca_core::Shared;

fn main() {
    let shared = Shared::new(0);
    let reader = shared.reader().unwrap();
    reader
        .try_update(
            |value| {
                *value = 1;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    let projection = reader.project(|value| Some(value));
    projection
        .try_update(
            |value| {
                *value = 1;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
}
