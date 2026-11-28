use rca_core::Shared;

fn main() {
    let shared = Shared::new(0);
    let reader = shared.reader().unwrap();
    reader.update(|value| *value = 1, || {}).unwrap();
    let projection = reader.project(|value| Some(value));
    projection.update(|value| *value = 1, || {}).unwrap();
}
