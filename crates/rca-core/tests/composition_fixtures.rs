//! Runs benchmark fixture contracts without compiling benchmark registrations.

#[expect(
    dead_code,
    reason = "the fixture tests do not run every benchmark helper"
)]
mod composition {
    include!("../benches/composition/fixtures.rs");
}
