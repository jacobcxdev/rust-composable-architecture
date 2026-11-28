//! Checks ordered ID selection independently of row values and insertion lifetimes.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};

use rca_core::{Effect, KeyedState, Reduce, Store};

use super::support::ManualRuntime;

/// Counts user ID operations without making the counter part of identity.
#[derive(Debug, Default)]
struct IdVisits {
    total: AtomicUsize,
    comparisons: AtomicUsize,
}

/// An ID whose copies, hashes and comparisons are visible to the test.
#[derive(Debug)]
struct CountedId {
    value: u8,
    visits: Arc<IdVisits>,
}

impl Clone for CountedId {
    fn clone(&self) -> Self {
        self.visits.total.fetch_add(1, Ordering::Relaxed);
        Self {
            value: self.value,
            visits: Arc::clone(&self.visits),
        }
    }
}

impl PartialEq for CountedId {
    fn eq(&self, other: &Self) -> bool {
        self.visits.total.fetch_add(1, Ordering::Relaxed);
        self.visits.comparisons.fetch_add(1, Ordering::Relaxed);
        self.value == other.value
    }
}

impl Eq for CountedId {}

impl Hash for CountedId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.visits.total.fetch_add(1, Ordering::Relaxed);
        self.value.hash(state);
    }
}

/// Separates row edits from changes visible to an ordered ID observer.
enum Edit {
    IncrementFirst,
    RestoreOrder,
    SwapLastTwo,
}

/// Records whether the Store signalled new projected state to a waiting reader.
struct Published(AtomicBool);

impl Wake for Published {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn child_edits_skip_ids_and_a_late_reorder_compares_the_sequence_once() {
    let visits = Arc::new(IdVisits::default());
    let rows: KeyedState<_, _> = [1, 2, 3, 4]
        .map(|value| {
            let id = CountedId {
                value,
                visits: Arc::clone(&visits),
            };
            (id, 0_u8)
        })
        .into_iter()
        .collect();
    let reducer = Reduce::new(|rows: &mut KeyedState<CountedId, u8>, edit: &Edit, _| {
        match edit {
            Edit::IncrementFirst => *rows.get_index_mut(0).expect("first row exists").1 += 1,
            Edit::RestoreOrder => {
                rows.reverse();
                rows.reverse();
            }
            Edit::SwapLastTwo => rows.move_index(2, 3),
        }
        Effect::none()
    });
    let runtime = ManualRuntime::new();
    let store = Store::new(rows, reducer, runtime.runtime());
    runtime.start_all();

    futures_executor::block_on(async {
        let mut ids = store
            .observe_ordered_ids(|rows| rows)
            .await
            .expect("registration");
        let initial = ids.next().await.expect("initial order");
        let published = Arc::new(Published(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&published));
        let mut next = std::pin::pin!(ids.next());
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        visits.total.store(0, Ordering::Relaxed);

        store
            .send(Edit::IncrementFirst)
            .await
            .expect("child edit")
            .finish()
            .await;
        assert_eq!(visits.total.load(Ordering::Relaxed), 0);
        assert!(!published.0.load(Ordering::Acquire));

        // Equal final order must not leave distinct comparison on an old allocation.
        store
            .send(Edit::RestoreOrder)
            .await
            .expect("restored order")
            .finish()
            .await;
        assert!(!published.0.load(Ordering::Acquire));
        visits.total.store(0, Ordering::Relaxed);
        store
            .send(Edit::IncrementFirst)
            .await
            .expect("second child edit")
            .finish()
            .await;
        assert_eq!(visits.total.load(Ordering::Relaxed), 0);
        assert!(!published.0.load(Ordering::Acquire));

        visits.comparisons.store(0, Ordering::Relaxed);
        store
            .send(Edit::SwapLastTwo)
            .await
            .expect("new order")
            .finish()
            .await;
        assert!(published.0.load(Ordering::Acquire));
        let reordered = next.await.expect("updated order");
        // Two equal positions and the first mismatch; a second scan would give six.
        assert_eq!(visits.comparisons.load(Ordering::Relaxed), 3);
        store.shutdown().await.expect("shutdown");
        assert!(initial.iter().map(|id| id.value).eq([1, 2, 3, 4]));
        assert!(reordered.iter().map(|id| id.value).eq([1, 2, 4, 3]));
    });
}

#[test]
fn selection_uses_public_ids_across_edits_and_independent_collections() {
    type Rows = KeyedState<u8, ()>;
    let runtime = ManualRuntime::new();
    let replace = Reduce::new(|rows: &mut Rows, next: &Rows, _| {
        *rows = next.clone();
        Effect::none()
    });
    let store = Store::new(Rows::new(), replace, runtime.runtime());
    runtime.start_all();

    let initial: Rows = [(1, ()), (2, ())].into_iter().collect();
    let mut reinserted = initial.clone();
    reinserted.remove(&2).expect("existing row");
    reinserted.insert(2, ());
    let mut fork = reinserted.clone();
    fork.insert(3, ());
    let mut other_fork = reinserted.clone();
    other_fork.insert(4, ());
    let independent = [(1, ()), (2, ()), (4, ())].into_iter().collect();
    let different = [(4, ()), (2, ()), (1, ())].into_iter().collect();

    futures_executor::block_on(async {
        let mut ids = store.observe_ordered_ids(|rows| rows).await.unwrap();
        let mut snapshots = vec![ids.next().await.unwrap()];
        for (rows, changed) in [
            (initial.clone(), true),
            (initial, false),
            (reinserted, false),
            (fork, true),
            (other_fork, true),
            (independent, false),
            (different, true),
            (Rows::new(), true),
        ] {
            let published = Arc::new(Published(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&published));
            let mut next = std::pin::pin!(ids.next());
            assert!(
                next.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            store.send(rows).await.unwrap().finish().await;
            assert_eq!(published.0.load(Ordering::Acquire), changed);
            if changed {
                snapshots.push(next.await.unwrap());
            }
        }
        store.shutdown().await.unwrap();
        let retained: Vec<_> = snapshots.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            retained,
            [&[][..], &[1, 2], &[1, 2, 3], &[1, 2, 4], &[4, 2, 1], &[]]
        );
    });
}
