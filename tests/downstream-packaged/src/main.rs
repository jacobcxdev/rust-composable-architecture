//! Checks registry-derived dependencies, cancellation, owned state and action receipts.

use std::time::Duration;

use rca::{DependencyKey, Effect, ReduceContext, Reducer, Store, TokioRuntime};

/// Supplies a scalar value through a derived dependency key.
struct ApiClient(u8);

/// Distinguishes live, test and explicitly overridden client values.
#[derive(DependencyKey)]
#[dependency(value = ApiClient, live = ApiClient(7), test = ApiClient(11))]
struct ApiClientKey;

/// Exercises dependency lookup and cancellable delayed work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    /// Reads the client value into state.
    Load,
    /// Schedules a delayed increment.
    Start,
    /// Cancels that scheduled increment.
    Stop,
    /// Applies an increment if delayed work reaches the reducer.
    Fired,
}

/// Identifies the timer independently of action values.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TimerId;

/// Implements the dependency and cancellation scenario.
struct Feature;

/// Keeps owned storage whose address must survive reducer execution.
struct ProductState {
    /// Contains the fixed payload used by state and receipt assertions.
    values: Box<[u8; 3]>,
}

/// Changes the owned payload without replacing its allocation.
enum ProductAction {
    /// Doubles each fixed input value.
    Double,
}

/// Mutates the existing owned payload in place.
struct ProductReducer;

impl Reducer for ProductReducer {
    type State = ProductState;
    type Action = ProductAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ProductAction::Double => {
                for value in state.values.iter_mut() {
                    *value *= 2;
                }
                Effect::none()
            }
        }
    }
}

/// Creates the common owned payload for state and receipt checks.
fn product_state() -> ProductState {
    ProductState {
        values: Box::new([1, 2, 3]),
    }
}

impl Reducer for Feature {
    type State = u8;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Load => {
                *state = context.dependency::<ApiClientKey>().0;
                Effect::none()
            }
            Action::Start => {
                Effect::send_after(Duration::from_secs(60), Action::Fired).cancellable(TimerId)
            }
            Action::Stop => Effect::cancel(TimerId),
            Action::Fired => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

/// Runs the packaged consumer with a current-thread Tokio runtime.
///
/// # Errors
/// Returns a runtime, store construction, admission or read error.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

/// Checks observation, cancellation and recovery of the original state allocation.
///
/// # Errors
/// Returns a runtime, store construction, admission or read error.
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::builder(0, Feature, TokioRuntime::current()?)
        .with_dependency::<ApiClientKey>(ApiClient(41))
        .build()?;
    let mut values = store.observe(|state| *state).await?;
    assert_eq!(values.next().await, Some(0));

    store.send(Action::Load).await?.finish().await;
    assert_eq!(values.next().await, Some(41));

    let timer = store.send(Action::Start).await?;
    store.send(Action::Stop).await?.finish().await;
    timer.finish().await;
    assert!(timer.is_finished());
    drop(values);
    store.shutdown().await?;

    let product = Store::new(product_state(), ProductReducer, TokioRuntime::current()?);
    let identity = product.read(|state| state.values.as_ptr() as usize).await?;
    product.send(ProductAction::Double).await?.finish().await;
    let state = product.shutdown_into_state().await?;
    assert_eq!(state.values.as_ref(), &[2, 4, 6]);
    assert_eq!(state.values.as_ptr() as usize, identity);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::{Future, IntoFuture};
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Poll, Waker};

    use rca::TestStore;

    use super::*;

    /// Carries a Send, non-Sync output without a Clone implementation.
    struct OwnedValue(Cell<u8>);

    /// Produces one optional owned output without requiring Sync.
    struct OwnedFuture(Cell<Option<u8>>);

    impl Future for OwnedFuture {
        type Output = Option<OwnedValue>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(self.0.take().map(|value| OwnedValue(Cell::new(value))))
        }
    }

    /// Separates future input, optional output and concatenated successor.
    enum FutureAction {
        /// Starts a future with the chosen optional value.
        Start(Option<u8>),
        /// Delivers the owned future result.
        Output(OwnedValue),
        /// Records progress after the future completes.
        Successor,
    }

    /// Counts mapper calls separately from committed state changes.
    struct FutureFeature(Arc<AtomicUsize>);

    impl Reducer for FutureFeature {
        type State = Vec<u8>;
        type Action = FutureAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            match action {
                FutureAction::Start(value) => {
                    let mappings = Arc::clone(&self.0);
                    Effect::concatenate([
                        Effect::future(OwnedFuture(Cell::new(*value))).map(move |value| {
                            mappings.fetch_add(1, Ordering::SeqCst);
                            FutureAction::Output(value)
                        }),
                        Effect::send(FutureAction::Successor),
                    ])
                }
                FutureAction::Output(value) => {
                    state.push(value.0.get());
                    Effect::none()
                }
                FutureAction::Successor => {
                    state.push(9);
                    Effect::none()
                }
            }
        }
    }

    // Dropping Some output in the interpreter must fail the first receive.
    #[tokio::test(flavor = "current_thread")]
    async fn packaged_owned_future_commits_mapped_output_before_successor() {
        let mappings = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(Vec::new(), FutureFeature(Arc::clone(&mappings)));
        let task = store.send(FutureAction::Start(Some(7))).await.unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, FutureAction::Output(value) if value.0.get() == 7),
                |state| *state = vec![7],
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, FutureAction::Successor),
                |state| *state = vec![7, 9],
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        assert_eq!(mappings.load(Ordering::SeqCst), 1);
        store.finish().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn packaged_none_future_completes_without_mapping_or_output() {
        let mappings = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(Vec::new(), FutureFeature(Arc::clone(&mappings)));
        let task = store.send(FutureAction::Start(None)).await.unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, FutureAction::Successor),
                |state| *state = vec![9],
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        assert_eq!(mappings.load(Ordering::SeqCst), 0);
        store.finish().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receipt_await_and_erasure_wait_for_committed_state() {
        let store = Store::new(
            product_state(),
            ProductReducer,
            TokioRuntime::current().unwrap(),
        );
        let receipt = store.try_send(ProductAction::Double).expect("admitted");
        let task = receipt.await.expect("direct receipt committed");
        assert!(task.is_finished());
        assert_eq!(store.read(|state| *state.values).await.unwrap(), [2, 4, 6]);

        let receipt = store.try_send(ProductAction::Double).expect("admitted");
        let future: rca::ActionReceiptFuture = receipt.into_future();
        let mut future = std::pin::pin!(future);
        // Current-thread execution has not yielded since admission. The actor
        // cannot have committed this command yet, so awaiting must still wait.
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let task = future.await.expect("concrete future committed");
        assert!(task.is_finished());

        let receipt = store.try_send(ProductAction::Double).expect("admitted");
        let erased: Pin<
            Box<dyn Future<Output = Result<rca::ActionTask, rca::StoreStopped>> + Send>,
        > = Box::pin(receipt.into_future());
        assert!(erased.await.expect("erased future committed").is_finished());
        let state = store.shutdown_into_state().await.unwrap();
        assert_eq!(*state.values, [8, 16, 24]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receipt_await_reports_owner_loss_before_commit() {
        let store = Store::new(
            product_state(),
            ProductReducer,
            TokioRuntime::current().unwrap(),
        );
        let receipt = store.try_send(ProductAction::Double).expect("admitted");
        drop(store);
        assert!(receipt.await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn packaged_test_store_controls_cancellation() {
        let mut store = TestStore::new(0, Feature);
        let load = store
            .send_with(Action::Load, |state| *state = 11)
            .await
            .expect("load commit");
        load.finish().await.expect("load completion");
        store.assert_state_eq(11).await.expect("test dependency");

        let timer = store.send(Action::Start).await.expect("timer commit");
        let stop = store.send(Action::Stop).await.expect("stop commit");
        stop.finish().await.expect("stop completion");
        timer.finish().await.expect("timer cancellation");
        assert!(timer.is_finished());
        store.finish().await.expect("clean finish");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finite_reducer_matches_projected_snapshot() {
        let mut store = TestStore::projected(product_state(), ProductReducer, |state| {
            state.values.to_vec()
        });
        let task = store
            .send_with(ProductAction::Double, |values| *values = vec![2, 4, 6])
            .await
            .expect("doubling commit");
        task.finish().await.expect("finite task completion");
        store.finish().await.expect("exhaustive finite reduction");
    }
}
