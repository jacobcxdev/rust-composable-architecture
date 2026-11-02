//! Saved root continuations own admission at their callback boundary.

use super::*;
use crate::store::lifecycle::CloseState;
use crate::store::transaction::tests::Host;
use crate::store::transaction::{self, Mutation};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Leaves state unchanged so only the saved reaction can select another value.
struct Root;
impl Feature for Root {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut crate::FeatureContext<'_, usize, ()>) {}
}

/// Compares captured values and counts their disposal independently.
struct Selected(usize, Arc<AtomicUsize>);
impl PartialEq for Selected {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Drop for Selected {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn saved_root_reaction_rechecks_admission_before_selector_entry() {
    let selections = Arc::new(AtomicUsize::new(0));
    let selected_count = std::sync::Arc::clone(&selections);
    let drops = Arc::new(AtomicUsize::new(0));
    let selected_drops = std::sync::Arc::clone(&drops);
    let factories = Arc::new(AtomicUsize::new(0));
    let factory_count = std::sync::Arc::clone(&factories);
    let hook = reaction::shared(
        move |value| {
            selected_count.fetch_add(1, Ordering::SeqCst);
            Selected(*value, std::sync::Arc::clone(&selected_drops))
        },
        InitialPolicy::Skip,
        move |_, _| {
            factory_count.fetch_add(1, Ordering::SeqCst);
            Task::run(|_| async {})
        },
    );
    let root = RootDeclaration::<usize, ()> {
        payload: Mutex::new(Some(RootInitial {
            hook: None,
            reactions: vec![hook],
            scope: None,
            pending: true,
            configuration: None,
            current_configuration: None,
        })),
    };
    let mut host = Host::<_, _, Shared>::new(4, Box::new(Root));
    host.mounts.push(Box::new(root));
    drop(transaction::commit(host.parts(), Mutation::Structural));

    let owner = &host.mounts[0];
    let mut frame = reaction::Frame::default();
    owner.capture_reactions(&host.state, &host.scopes, &mut frame);
    let captured = frame
        .take(owner.registration_identity().unwrap(), 0, &host.scopes)
        .unwrap();
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // Shutdown may occur after extraction; the saved callback remains responsible
    // for admission before entering its selector, not only after selection.
    assert!(host.lifecycle.close(CloseState::Abortive));
    let mut transitions = ScopeTransitionBatch::default();
    let context = ReduceContext::new(
        &mut host.dependencies,
        crate::MonotonicInstant::ZERO,
        &mut host.scopes,
        &mut transitions,
    );
    assert!(captured(&5, context).is_none());
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
