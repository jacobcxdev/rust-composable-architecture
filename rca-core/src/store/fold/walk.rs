//! One iterative preorder fold for both execution domains.
//!
//! Only explicit transforms create groups. A group consumes its owned values
//! in roster order, then contributes one transformed value to its parent.
//! Before the next callback or success, the walker checks for Store closure.
//! Error and panic paths drop retained values without resuming traversal.

use super::{CallbackLease, FoldKey};
use crate::store::carriers::Carriers;
use crate::store::lifecycle::Lifecycle;
use crate::store::mount::MountDeclaration;
use crate::store::scope::ScopeRegistry;
use crate::store::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
use std::ops::ControlFlow;

/// Selects one current incarnation and its contiguous preorder descendants.
///
/// Frozen authority is checked before roster lookup. The roster owns each
/// declaration throughout selection, and no user selector runs here.
pub(super) fn subtree<'a, R, A, C: Carriers<R, A>>(
    scopes: &ScopeRegistry,
    mounts: &'a [Box<C::Mount>],
    identity: usize,
    frozen: &super::super::output::FrozenRegistration,
) -> Result<&'a [Box<C::Mount>], TaskAccessError> {
    let start = if frozen.is_current(scopes) {
        mounts
            .iter()
            .position(|owner| owner.registration_identity() == Some(identity))
    } else {
        None
    }
    .ok_or_else(|| TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted))?;
    let depth = mounts[start].depth();
    let length = 1 + mounts[start + 1..]
        .iter()
        .take_while(|owner| owner.depth() > depth)
        .count();
    Ok(&mounts[start..start + length])
}

/// Keeps one explicit transform group open until traversal leaves its subtree.
struct Frame<K: FoldKey, L> {
    /// The declaration depth that bounds this group's contiguous descendants.
    depth: usize,
    /// Taken only while reduction or transformation owns the value.
    accumulator: Option<K::Value>,
    /// Skips remaining descendants without skipping this group's final transform.
    broken: bool,
    /// Retains detached callbacks until transformation and capture restoration finish.
    entry: L,
}

/// Checks closure after entry, preserving conservative user-execution progress.
pub(super) fn live(lifecycle: &Lifecycle) -> Result<(), TaskAccessError> {
    lifecycle
        .access()
        .map_err(|error| TaskAccessError::new(error.reason(), TaskProgress::MayHaveRun))
}

/// Moves one contribution into its group and records an explicit break.
///
/// The accumulator is absent during user reduction. Panic unwinds the walk
/// rather than resuming it with a missing value.
fn feed<K: FoldKey>(
    accumulator: &mut Option<K::Value>,
    next: K::Value,
    lifecycle: &Lifecycle,
) -> Result<bool, TaskAccessError> {
    live(lifecycle)?;
    let (value, broken) = match K::reduce(accumulator.take().expect("one owned accumulator"), next)
    {
        ControlFlow::Continue(value) => (value, false),
        ControlFlow::Break(value) => (value, true),
    };
    *accumulator = Some(value);
    Ok(broken)
}

/// Transforms the innermost group and feeds it to its parent or root.
///
/// The caller supplies a nonempty stack. The lease returns before parent
/// reduction, whose lifecycle check also follows transform and lease destruction.
/// The result reports a root break. A nested break stays on its parent frame.
fn finish_frame<R, K: FoldKey, L: CallbackLease<R, K>>(
    frames: &mut Vec<Frame<K, L>>,
    accumulator: &mut Option<K::Value>,
    lifecycle: &Lifecycle,
) -> Result<bool, TaskAccessError> {
    live(lifecycle)?;
    let mut frame = frames.pop().unwrap();
    let value = frame
        .entry
        .entry()
        .transform(frame.accumulator.take().unwrap());
    drop(frame);
    if let Some(parent) = frames.last_mut() {
        parent.broken = feed::<K>(&mut parent.accumulator, value, lifecycle)?;
        Ok(false)
    } else {
        feed::<K>(accumulator, value, lifecycle)
    }
}

/// Folds committed declarations without recursive calls or owned state projections.
///
/// The actor owns the roster throughout this synchronous walk. Acquisition
/// detaches callbacks without entering user code or yielding to another command.
/// Only open transform groups occupy the stack, so storage follows group depth.
/// Every successful result follows a final lifecycle check after user callbacks.
pub(super) fn walk<R: 'static, A, K: FoldKey, C: Carriers<R, A>, L: CallbackLease<R, K>>(
    root: &R,
    scopes: &ScopeRegistry,
    mounts: &[Box<C::Mount>],
    lifecycle: &Lifecycle,
    mut acquire: impl FnMut(usize) -> Option<L>,
) -> Result<K::Value, TaskAccessError> {
    live(lifecycle)?;
    let mut accumulator = Some(K::identity());
    let mut frames: Vec<Frame<K, L>> = Vec::new();
    let mut broken = false;
    for owner in mounts {
        let depth = owner.depth();
        // Finish groups before considering this sibling. A broken group still
        // transforms once and can break its enclosing group during reduction.
        while frames.last().is_some_and(|frame| depth <= frame.depth) {
            broken |= finish_frame::<R, K, L>(&mut frames, &mut accumulator, lifecycle)?;
        }
        if broken {
            break;
        }
        if frames.last().is_some_and(|frame| frame.broken) {
            continue;
        }
        let Some(identity) = owner.registration_identity() else {
            continue;
        };
        let Some(_) = owner
            .installed_scope()
            .filter(|scope| scopes.is_current(*scope))
        else {
            continue;
        };
        live(lifecycle)?;
        // The actor-owned roster keeps this registration and scope installed
        // throughout acquisition; no callback or suspension intervenes.
        let Some(entry) = acquire(identity) else {
            continue;
        };
        live(lifecycle)?;
        let value = if entry.entry().has_transform() {
            frames.push(Frame {
                depth,
                accumulator: Some(K::identity()),
                broken: false,
                entry,
            });
            live(lifecycle)?;
            frames
                .last()
                .unwrap()
                .entry
                .entry()
                .contribution(root, lifecycle)?
        } else {
            entry.entry().contribution(root, lifecycle)?
        };
        if let Some(value) = value {
            if let Some(frame) = frames.last_mut() {
                frame.broken = feed::<K>(&mut frame.accumulator, value, lifecycle)?;
            } else {
                broken = feed::<K>(&mut accumulator, value, lifecycle)?;
            }
        }
    }
    // Remaining groups still transform after a nested break. No later root
    // sibling remains, so the final root-break result needs no further action.
    while !frames.is_empty() {
        let _ = finish_frame::<R, K, L>(&mut frames, &mut accumulator, lifecycle)?;
    }
    live(lifecycle)?;
    Ok(accumulator.unwrap())
}

#[cfg(test)]
ordinary_test_items! {
    mod tests {
        use super::walk;
        use crate::store::carriers::Shared;
        use crate::store::fold::{EntryLease, Fold, FoldKey};
        use crate::store::lifecycle::CloseState;
        use crate::store::mount;
        use crate::store::scope::ScopeRegistry;
        use crate::store::task_context::{TaskAccessReason, TaskProgress};
        use crate::store::transaction::tests::Host;
        use crate::store::transaction::{self, Mutation};
        use crate::{Feature, FeatureContext, MountIdentity, OptionalMount};
        use std::ops::ControlFlow;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Unentered;

        impl FoldKey for Unentered {
            type Value = ();

            fn identity() {
                panic!("closed walk must not enter identity");
            }

            fn reduce((): (), (): ()) -> ControlFlow<(), ()> {
                panic!("closed walk must not enter reduction");
            }
        }

        #[test]
        fn closure_after_admission_prevents_walk_entry() {
            let (lifecycle, scopes) = ScopeRegistry::for_test();
            lifecycle.access().expect("walk begins with an open lifecycle");
            // The real lifecycle can close after the caller's admission check and
            // before the walker starts, even when there are no mounted children.
            assert!(lifecycle.close(CloseState::Abortive));
            let error = walk::<(), (), Unentered, Shared, EntryLease<(), Unentered>>(
                &(),
                &scopes,
                &[],
                &lifecycle,
                |_| panic!("closed walk must not acquire a callback"),
            )
            .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        }

        struct Root;

        impl Feature for Root {
            type State = Option<usize>;
            type Action = ();

            fn update(
                &mut self,
                _: &mut Self::State,
                (): &(),
                _: &mut FeatureContext<'_, Self::State, ()>,
            ) {
            }
        }

        struct Child;

        impl Feature for Child {
            type State = usize;
            type Action = ();

            fn update(
                &mut self,
                _: &mut usize,
                (): &(),
                _: &mut FeatureContext<'_, usize, ()>,
            ) {
            }
        }

        #[test]
        fn closure_after_real_acquisition_prevents_subtree_identity() {
            static IDENTITIES: AtomicUsize = AtomicUsize::new(0);

            struct CountedIdentity;

            impl FoldKey for CountedIdentity {
                type Value = usize;

                fn identity() -> usize {
                    IDENTITIES.fetch_add(1, Ordering::SeqCst);
                    0
                }

                fn reduce(_: usize, _: usize) -> ControlFlow<usize, usize> {
                    panic!("closed acquisition must not enter reduction");
                }
            }

            let mut host = Host::<_, (), Shared>::new(Some(3), Box::new(Root));
            let (owner, site) = mount::attach(OptionalMount::new(
                Option::as_ref,
                Option::as_mut,
                |state: &Option<usize>| state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                }),
                |_| Child,
            ));
            owner.bind(&host.ingress, &host.lifecycle);
            host.mounts.push(owner);
            let fold = Fold::<_, CountedIdentity>::new();
            let entry = fold.attach(
                &site,
                Some(Box::new(|_| {
                    panic!("closed acquisition must not enter contribution")
                })),
                Some(Box::new(|_| {
                    panic!("closed acquisition must not enter transform")
                })),
            );
            host.mounts.push(Box::new(entry));
            drop(transaction::commit(host.parts(), Mutation::Structural));
            let mut acquired = false;
            let error = walk::<_, (), CountedIdentity, Shared, _>(
                &host.state,
                &host.scopes,
                &host.mounts,
                &host.lifecycle,
                |identity| {
                    let lease = fold.acquire(identity).expect("real committed lease");
                    acquired = true;
                    // Model closure immediately after acquiring the real callback;
                    // acquisition itself need not execute any user callback.
                    assert!(host.lifecycle.close(CloseState::Abortive));
                    Some(lease)
                },
            )
            .unwrap_err();
            assert!(acquired);
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!(IDENTITIES.load(Ordering::SeqCst), 1);
        }
    }
}
