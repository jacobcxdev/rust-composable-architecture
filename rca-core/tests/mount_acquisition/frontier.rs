//! The real acquisition call revalidates after the host's metadata precheck.
use super::*;
use rca_core::{TaskAccessReason, TaskProgress, testing::TransactionGate};

struct State {
    child: Option<usize>,
    replacement: u64,
    root_configuration: u64,
}
struct Root;
impl Feature for Root {
    type State = State;
    type Action = u8;
    fn update(&mut self, state: &mut State, action: &u8, _: &mut FeatureContext<'_, State, u8>) {
        if *action == 1 {
            state.root_configuration += 1;
        } else {
            state.replacement += 1;
        }
    }
}
struct Child;
impl Feature for Child {
    type State = usize;
    type Action = u8;
    fn update(&mut self, state: &mut usize, action: &u8, _: &mut FeatureContext<'_, usize, u8>) {
        *state += usize::from(*action);
    }
}
type Site = rca_core::MountSite<State, usize, u8, u8>;
/// Installs a child and acknowledges its initial Structural boundary.
fn mounted(driver: &Driver, gate: &TransactionGate<State, u8>) -> (Store<State, u8>, Site) {
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, site) = rca_core::RootMount::new(Root)
        .configuration(|state: &State| state.root_configuration)
        .builder(
            State {
                child: Some(3),
                replacement: 0,
                root_configuration: 0,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(descriptor);
    let store = builder.transaction_gate(gate.clone()).build().unwrap();
    driver.turn();
    release(driver, gate);
    (store, site)
}
/// Releases and acknowledges the exact held boundary before the next turn.
fn release(driver: &Driver, gate: &TransactionGate<State, u8>) {
    let boundary = gate.boundary().unwrap();
    let mut receipt = Box::pin(gate.release_pending_with_receipt().unwrap().finish());
    driver.turn();
    assert!(matches!(poll(receipt.as_mut()), Poll::Ready(Ok(_))));
    assert!(gate.acknowledge(boundary));
    driver.turn();
}

#[test]
fn replacement_after_precheck_acquires_the_new_current_incarnation() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let (store, site) = mounted(&driver, &gate);
    gate.validate_acquisition(&store, &site)
        .expect("current mount passes the acquisition precheck");
    let mut replace = Box::pin(store.send(0));
    assert!(poll(replace.as_mut()).is_pending());
    driver.turn();
    release(&driver, &gate);
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    let mut acquire = Box::pin(gate.acquire(&store, &site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(context))) = poll(acquire.as_mut()) else {
        panic!("fresh context");
    };
    let mut send = Box::pin(context.send(1));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.inspect_mounted(&context, |action| *action),
        Ok(Some(1))
    );
    release(&driver, &gate);
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
}

#[test]
fn foreign_gate_pairing_is_rejected_at_the_actual_query_execution() {
    let driver = Driver::default();
    let own = TransactionGate::new();
    let foreign = TransactionGate::new();
    let (store, site) = mounted(&driver, &own);
    let (other, _) = mounted(&driver, &foreign);
    assert!(
        foreign.validate_acquisition(&store, &site).is_ok(),
        "precheck has actual source metadata"
    );
    let mut held = Box::pin(other.send(0));
    assert!(poll(held.as_mut()).is_pending());
    driver.turn();
    let boundary = foreign.boundary();
    let mut acquire = Box::pin(foreign.acquire(&store, &site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Err(error)) = poll(acquire.as_mut()) else {
        panic!("foreign actor rejects metadata");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(foreign.boundary(), boundary);
    release(&driver, &foreign);
    assert!(matches!(poll(held.as_mut()), Poll::Ready(Ok(_))));
}

#[test]
fn close_after_precheck_is_revalidated_without_running_a_query() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let (store, site) = mounted(&driver, &gate);
    gate.validate_acquisition(&store, &site)
        .expect("open store passes the acquisition precheck");
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    let mut acquire = Box::pin(gate.acquire(&store, &site));
    let Poll::Ready(Err(error)) = poll(acquire.as_mut()) else {
        panic!("closed lifecycle");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
}

#[test]
fn same_leaf_with_stale_root_configuration_cannot_inspect_the_current_request() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let (store, site) = mounted(&driver, &gate);
    let acquire = || {
        let mut request = Box::pin(gate.acquire(&store, &site));
        assert!(poll(request.as_mut()).is_pending());
        driver.turn();
        let Poll::Ready(Ok(Some(context))) = poll(request.as_mut()) else {
            panic!("current context");
        };
        context
    };
    let stale = acquire();
    let mut configure = Box::pin(store.send(1));
    assert!(poll(configure.as_mut()).is_pending());
    driver.turn();
    release(&driver, &gate);
    assert!(matches!(poll(configure.as_mut()), Poll::Ready(Ok(_))));
    let current = acquire();
    let mut send = Box::pin(current.send(2));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    let error = gate
        .inspect_mounted(&stale, |_| panic!("ancestor-stale matcher"))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(
        gate.inspect_mounted(&current, |action| *action),
        Ok(Some(2))
    );
    release(&driver, &gate);
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
}

#[test]
fn diagnostic_acquisition_reads_held_incarnation_while_ordinary_acquisition_keeps_fifo() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let (store, site) = mounted(&driver, &gate);
    let mut replacement = Box::pin(store.send(0));
    assert!(poll(replacement.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary();
    let mut ordered = Box::pin(store.acquire(&site));
    assert!(poll(ordered.as_mut()).is_pending());
    let mut diagnostic = Box::pin(gate.acquire(&store, &site));
    assert!(poll(diagnostic.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(old))) = poll(diagnostic.as_mut()) else {
        panic!("diagnostic observes the held incarnation");
    };
    assert_eq!(gate.boundary(), boundary);
    assert!(poll(ordered.as_mut()).is_pending());
    assert!(poll(replacement.as_mut()).is_pending());
    release(&driver, &gate);
    assert!(matches!(poll(replacement.as_mut()), Poll::Ready(Ok(_))));
    let Poll::Ready(Ok(Some(current))) = poll(ordered.as_mut()) else {
        panic!("ordinary acquisition follows replacement in ingress order");
    };
    let mut stale = Box::pin(old.read(|_| panic!("retired diagnostic context")));
    assert!(poll(stale.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Err(error)) = poll(stale.as_mut()) else {
        panic!("diagnostic did not observe replacement early");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    let mut read = Box::pin(current.read(|state| *state));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert_eq!(poll(read.as_mut()), Poll::Ready(Ok(3)));
}

#[test]
fn closed_foreign_context_cannot_inspect_or_disturb_a_live_mounted_frontier() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let (store, site) = mounted(&driver, &gate);
    let foreign_gate = TransactionGate::new();
    let (foreign_store, foreign_site) = mounted(&driver, &foreign_gate);
    let mut acquire = Box::pin(foreign_gate.acquire(&foreign_store, &foreign_site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(closed))) = poll(acquire.as_mut()) else {
        panic!("foreign mounted context");
    };
    drop(acquire);
    drop(foreign_store);
    driver.turn();
    let mut acquire = Box::pin(gate.acquire(&store, &site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(current))) = poll(acquire.as_mut()) else {
        panic!("live mounted context");
    };
    let mut send = Box::pin(current.send(2));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    let error = gate
        .inspect_mounted(&closed, |_| panic!("closed foreign matcher must not run"))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(gate.has_pending());
    assert!(!gate.has_failed_match());
    assert!(poll(send.as_mut()).is_pending());
    assert_eq!(
        gate.inspect_mounted(&current, |action| *action),
        Ok(Some(2))
    );
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(gate.has_pending());
    release(&driver, &gate);
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut read = Box::pin(current.read(|state| *state));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert_eq!(poll(read.as_mut()), Poll::Ready(Ok(5)));
    drop(store);
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
}
