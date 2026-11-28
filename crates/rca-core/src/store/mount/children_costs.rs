//! Actual opt-in declaration/reconciliation phases, not an alternative kernel.
//!
//! The allocation host supplies the profiler. Captured counters precede each
//! measured attachment. Cold and warm passes use the same real declaration owners.
mod initial;
use super::allocation_tests::measure;
use super::{
    Arc, Command, Feature, MountIdentity, OptionalMount, ScopeRegistry, ScopeTransitionBatch,
    Shared,
};
use crate::effect::lineage::ControlWake;
use crate::store::ingress::CommandIngress;
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Supplies one shared leaf projection to distinct child declarations.
struct ParentState {
    child: Option<usize>,
}
/// Retains reducer-local state without adding work to structural reconciliation.
struct Parent(Cell<usize>);
impl Feature for Parent {
    type State = ParentState;
    type Action = bool;
    fn update(
        &mut self,
        _: &mut ParentState,
        _: &bool,
        _: &mut crate::FeatureContext<'_, ParentState, bool>,
    ) {
        self.0.set(self.0.get() + 1);
    }
}
/// Provides an inert installed leaf for declaration-storage measurements.
struct Leaf;
impl Feature for Leaf {
    type State = usize;
    type Action = u8;
    fn update(&mut self, _: &mut usize, _: &u8, _: &mut crate::FeatureContext<'_, usize, u8>) {}
}
/// Lets the actual optional mount select the fixture's parent.
type State = Option<ParentState>;
/// Creates the same parent declaration for flat and child-recipe cases.
fn descriptor() -> OptionalMount<State, Parent, ()> {
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &State| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Parent(Cell::new(0)),
    )
}

/// Compares attachment, reconciliation and owner destruction under an existing profiler.
pub(in crate::store) fn run() {
    initial::run();
    for children in [None, Some(0), Some(1), Some(64)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let recipe_calls = std::sync::Arc::clone(&calls);
        let ((owner, site), attach) = measure(|| match children {
            None => super::attach::<State, (), _, _>(descriptor()),
            Some(count) => {
                super::children::attach::<State, (), _, _>(descriptor(), move |_, children| {
                    recipe_calls.fetch_add(1, Ordering::Relaxed);
                    // Distinct declarations deliberately project the same leaf in
                    // this storage assay; this is not a keyed-collection workload.
                    for _ in 0..count {
                        children.optional(OptionalMount::new(
                            |state: &ParentState| state.child.as_ref(),
                            |state: &mut ParentState| state.child.as_mut(),
                            |state| {
                                state.child.map(|_| MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            |_| Leaf,
                        ));
                    }
                })
            }
        });
        let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let ingress = Arc::new(CommandIngress::<Command<State, ()>>::new(
            sender,
            ControlWake::new(),
        ));
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        owner.bind(&ingress, &lifecycle);
        let mut mounts = vec![owner];
        let mut mounted = Vec::new();
        let mut transitions = ScopeTransitionBatch::default();
        let state = Some(ParentState { child: Some(7) });
        let (cold, warm) = {
            let mut reconcile = || {
                super::nested::reconcile_roster::<State, (), Shared>(
                    &mut mounts,
                    children.is_some(),
                    &state,
                    &mut scopes,
                    &mut transitions,
                    &mut mounted,
                    #[cfg(feature = "diagnostics")]
                    None,
                );
            };
            let ((), cold) = measure(&mut reconcile);
            let ((), warm) = measure(&mut reconcile);
            (cold, warm)
        };
        assert_eq!(mounts.len(), 1 + children.unwrap_or(0));
        assert_eq!(mounted.len(), 1 + children.unwrap_or(0));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            usize::from(children.is_some())
        );
        let ((), owner_drop) = measure(|| drop((mounts, mounted)));
        eprintln!(
            "public child recipe {children:?} actual attach/cold/warm/owner-drop blocks/bytes={attach:?}/{cold:?}/{warm:?}/{owner_drop:?}; recipe calls={}",
            calls.load(Ordering::Relaxed)
        );
        drop(site);
    }
}
