//! Real endpoints share only metadata and release it with their last owner.

#[cfg(feature = "local")]
use super::erased_path::LocalPath;
use super::erased_path::SharedPath;
use super::path::BorrowPath;
use super::*;
use crate::Reduce;
use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot};
use crate::store::transaction::tests::Host;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
/// Separates metadata sharing from installed Feature lifetime.
struct Counts {
    /// Detects cloning the underlying path instead of its shared owner.
    clones: AtomicUsize,
    /// Counts destruction of the single shared path payload.
    metadata_drops: AtomicUsize,
    /// Counts Feature destruction independently of retained endpoint metadata.
    feature_drops: AtomicUsize,
}

/// Exposes deep clones and drops behind a Shared or Local erased path.
struct TrackedPath<M> {
    /// Observations retained by the test after every path owner is gone.
    counts: Arc<Counts>,
    /// Forces non-Send ownership in the Local instantiation.
    marker: M,
}
impl<M: Clone> Clone for TrackedPath<M> {
    fn clone(&self) -> Self {
        self.counts.clones.fetch_add(1, Ordering::SeqCst);
        Self {
            counts: Arc::clone(&self.counts),
            marker: self.marker.clone(),
        }
    }
}
impl<M> Drop for TrackedPath<M> {
    fn drop(&mut self) {
        self.counts.metadata_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl<M: Clone> BorrowPath<usize> for TrackedPath<M> {
    type State = usize;
    fn read<'a>(&self, root: &'a usize) -> Option<&'a usize> {
        Some(root)
    }
    fn write<'a>(&self, root: &'a mut usize) -> Option<&'a mut usize> {
        Some(root)
    }
}
/// Observes retirement without performing work during the metadata read.
struct Child(Arc<Counts>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.feature_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Feature for Child {
    type State = usize;
    type Action = u8;
    fn update(&mut self, _: &mut usize, _: &u8, _: &mut FeatureContext<'_, usize, u8>) {}
}
#[cfg(feature = "local")]
/// Prevents the Local fixture from passing through accidental Send compatibility.
struct LocalChild {
    /// Reuses the same independent Feature-destruction observation.
    child: Child,
    /// Keeps this fixture non-Send even though its counters use atomics.
    _local: std::rc::Rc<()>,
}
#[cfg(feature = "local")]
impl local::Feature for LocalChild {
    type State = usize;
    type Action = u8;
    fn update(&mut self, _: &mut usize, _: &u8, _: &mut local::FeatureContext<'_, usize, u8>) {
        let _ = &self.child;
    }
}

// A deep-copy Clone, a retained strong Feature node, or leaked final endpoint
// would fail these independent counters. No allocator result stands in for Drop.
macro_rules! lifetime {
    ($name:ident, $carrier:ty, $driver:expr, $path:expr, $node:expr, $context:expr) => {
        #[test]
        fn $name() {
            for siblings in [1, 64] {
                for retain_future in [false, true] {
                    let counts = Arc::new(Counts::default());
                    let mut host = Host::<usize, (), $carrier>::new(7, $driver);
                    let token = host.scopes.mount(
                        host.scopes.root(),
                        ScopeRegistry::node(&mut ScopeSlot::new()),
                        ScopeIdentity::new("metadata lifetime"),
                    );
                    let node = ($node)(Arc::clone(&counts), token);
                    let path = ($path)(Arc::clone(&counts));
                    let mut contexts = (0..siblings)
                        .map(|_| ($context)(&node, path.clone(), &host))
                        .collect::<Vec<_>>();
                    drop(path);
                    host.mounted.push(Box::new(node));
                    let last = contexts.pop().unwrap();
                    let mut read = Box::pin(last.read(|value| *value));
                    assert!(
                        read.as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                    drop(host.next());
                    assert!(matches!(
                        read.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                        Poll::Ready(Ok(7))
                    ));
                    drop(read);
                    drop(contexts);
                    drop(host);
                    assert_eq!(
                        counts.feature_drops.load(Ordering::SeqCst),
                        1,
                        "retained contexts cannot retain the installed Feature"
                    );
                    assert_eq!(
                        counts.clones.load(Ordering::SeqCst),
                        0,
                        "sibling endpoints and submitted reads share the same metadata payload"
                    );
                    assert_eq!(counts.metadata_drops.load(Ordering::SeqCst), 0);
                    if retain_future {
                        let pending = last.read(|value| *value);
                        drop(last);
                        assert_eq!(
                            counts.metadata_drops.load(Ordering::SeqCst),
                            0,
                            "an owned unpolled request still retains its route metadata"
                        );
                        drop(pending);
                    } else {
                        drop(last);
                    }
                    assert_eq!(
                        counts.metadata_drops.load(Ordering::SeqCst),
                        1,
                        "the last context or owned request releases the sole metadata payload"
                    );
                    assert_eq!(counts.feature_drops.load(Ordering::SeqCst), 1);
                }
            }
        }
    };
}

lifetime!(
    shared_metadata_lifetime_follows_last_context_not_feature,
    Shared,
    Box::new(Reduce::new(
        |_: &mut usize, (): &(), _: &mut ReduceContext<'_>| crate::Effect::none()
    )),
    |counts| SharedPath::new(TrackedPath { counts, marker: () }),
    |counts, token| SharedNode::new(Child(counts), token, 0),
    |node: &SharedNode<Child>, path: SharedPath<usize, usize>, host: &Host<usize, (), Shared>| {
        node.downgrade().context::<usize, ()>(
            path,
            std::sync::Weak::clone(&host.weak),
            &host.scopes,
        )
    }
);

#[cfg(feature = "local")]
lifetime!(
    local_metadata_lifetime_follows_last_context_not_feature,
    local::Local,
    Box::new(local::Driver(local::Reduce::new(
        |_: &mut usize, (): &(), _: &mut ReduceContext<'_>| local::Effect::none()
    ))),
    |counts| LocalPath::new(TrackedPath {
        counts,
        marker: std::rc::Rc::new(())
    }),
    |counts, token| LocalNode::new(
        LocalChild {
            child: Child(counts),
            _local: std::rc::Rc::new(())
        },
        token,
        0
    ),
    |node: &LocalNode<LocalChild>,
     path: LocalPath<usize, usize>,
     host: &Host<usize, (), local::Local>| {
        node.context::<usize, ()>(
            path,
            std::sync::Weak::clone(&host.weak),
            Provenance::External { handle: None },
            host.scopes.authority(node.owned.borrow().token),
        )
    }
);
