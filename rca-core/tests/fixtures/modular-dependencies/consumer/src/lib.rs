//! Consumes only the interface, with no production provider dependency.

use modular_interface::ClientKey;
use rca_core::{Effect, ReduceContext, Reducer};

/// Reads the selected interface dependency during real reduction.
#[derive(Debug)]
pub struct Feature;

impl Reducer for Feature {
    type State = &'static str;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        *state = context.dependency::<ClientKey>().call();
        Effect::none()
    }
}

#[test]
fn interface_only_consumer_runs_a_real_reducer_with_owned_test_defaults() {
    use modular_interface::Client;
    use rca_core::DependencyDefaults;
    use rca_core::DependencyValues;
    use rca_test::TestStore;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let calls = Arc::new(AtomicUsize::new(0));
    let client = Client::new({
        let calls = Arc::clone(&calls);
        move || {
            calls.fetch_add(1, Ordering::Relaxed);
            "consumer fixture"
        }
    });
    assert_eq!(format!("{client:?}"), "Client { .. }");
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let mut values = DependencyValues::new();
    values
        .register_defaults(DependencyDefaults::<ClientKey>::new().with_test(move || client.clone()))
        .unwrap();

    futures_executor::block_on(async {
        let mut store = TestStore::builder("", Feature)
            .with_dependency_values(values)
            .build();
        let _ = store
            .send_with((), |state| *state = "consumer fixture")
            .await
            .unwrap();
        store.finish().await.unwrap();
    });

    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
