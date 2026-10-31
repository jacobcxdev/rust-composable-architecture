//! Checks generated client defaults, configuration isolation and callback ownership.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};

use rca_macros::DependencyClient;

/// Matches the owned asynchronous result accepted by a generated callback field.
type Response = Pin<Box<dyn Future<Output = String> + Send>>;

/// Deliberately collides with the generated call-site variable's preferred name.
#[expect(
    non_upper_case_globals,
    reason = "regression for a caller-owned identifier"
)]
const __rca_dependency_location: usize = 7;

/// Uses a caller-owned identifier in both stored data and a configured callback.
#[derive(DependencyClient)]
struct HygienicClient {
    /// Receives the caller's constant without generated-name shadowing.
    #[dependency(member = __rca_dependency_location)]
    value: usize,
    /// Returns the same caller-owned constant through a callback expression.
    #[dependency(test = Arc::new(|| __rca_dependency_location))]
    configured: Arc<dyn Fn() -> usize + Send + Sync>,
    /// Retains the generated failure and configuration-location path.
    unconfigured: Arc<dyn Fn() + Send + Sync>,
}

#[test]
fn generated_location_does_not_capture_or_shadow_caller_identifiers() {
    let line = line!() + 1;
    let client = HygienicClient::test_value();
    assert_eq!(client.value, 7);
    assert_eq!((client.configured)(), 7);
    let panic = catch_unwind(AssertUnwindSafe(|| (client.unconfigured)()))
        .expect_err("unconfigured endpoint");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
}

/// Mixes generated failures with an explicit default and owned callback overrides.
#[derive(Clone, DependencyClient)]
struct Client {
    /// Is overridden independently by each test configuration.
    load: Arc<dyn Fn(String, usize) -> String + Send + Sync>,
    /// Must fail at invocation when no asynchronous implementation is configured.
    async_load: Arc<dyn Fn(String) -> Response + Send + Sync>,
    /// An unrelated field attribute must not become a dependency option.
    #[dependency(test = Arc::new(|| 17))]
    #[doc = "Attributes after the dependency option must not replace it either."]
    count: Arc<dyn Fn() -> usize + Send + Sync>,
}

#[test]
fn selective_override_preserves_explicit_default() {
    let client = Client {
        load: Arc::new(|name, count| name.repeat(count)),
        ..Client::test_value()
    };
    assert_eq!((client.load)(String::from("x"), 3), "xxx");
    assert_eq!((client.count)(), 17);
}

#[test]
fn unconfigured_endpoints_fail_on_invocation_without_polling() {
    let client = Client::test_value();
    let load = catch_unwind(AssertUnwindSafe(|| (client.load)(String::new(), 0)))
        .expect_err("unconfigured synchronous endpoint");
    let async_load = catch_unwind(AssertUnwindSafe(|| {
        let _future = (client.async_load)(String::new());
    }))
    .expect_err("unconfigured async endpoint fails before polling");
    for (panic, endpoint) in [(load, ".load"), (async_load, ".async_load")] {
        let message = panic.downcast_ref::<String>().expect("panic text");
        assert!(message.contains("dependency_client::Client"), "{message}");
        assert!(message.contains(endpoint), "{message}");
    }
}

#[test]
fn cloned_client_owns_its_endpoint_after_original_is_dropped() {
    let captured = Arc::new(String::from("owned"));
    let weak = Arc::downgrade(&captured);
    let client = Client {
        load: Arc::new(move |_, _| captured.as_ref().clone()),
        ..Client::test_value()
    };
    let clone = client.clone();
    drop(client);
    assert_eq!((clone.load)(String::new(), 0), "owned");
    assert!(weak.upgrade().is_some());
    drop(clone);
    assert!(weak.upgrade().is_none());
}

#[test]
fn parallel_configurations_do_not_share_endpoint_overrides() {
    let barrier = Arc::new(Barrier::new(2));
    let threads = ["first", "second"].map(|label| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let client = Client {
                load: Arc::new(move |_, _| label.to_owned()),
                ..Client::test_value()
            };
            barrier.wait();
            (client.load)(String::new(), 0)
        })
    });
    // Join both workers before propagating either panic.
    let results = threads.map(std::thread::JoinHandle::join);
    let [first, second] = results.map(Result::unwrap);
    assert_eq!(first, "first");
    assert_eq!(second, "second");
}

/// Deliberately supplies neither Clone, Debug nor Send for input and member checks.
struct LocalInput(std::rc::Rc<String>);

/// Keeps generic callback values separate from local move-only stored members.
#[derive(DependencyClient)]
struct MixedClient<T> {
    /// Retains the caller's type parameter without storing a value of that type.
    #[dependency(member = std::marker::PhantomData)]
    marker: std::marker::PhantomData<T>,
    /// Is moved into the identity callback rather than cloned.
    #[dependency(member = LocalInput(std::rc::Rc::new(String::from("member"))))]
    local: LocalInput,
    /// Returns the exact input type without adding ownership bounds to it.
    #[dependency(test = Arc::new(|input| input))]
    identity: Arc<dyn Fn(T) -> T + Send + Sync>,
    /// Must report a missing implementation rather than invent an error value.
    fallible: Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>,
}

#[test]
fn explicit_members_preserve_local_move_only_inputs_and_generics() {
    let client = MixedClient::<LocalInput>::test_value();
    let output = (client.identity)(client.local);
    assert_eq!(output.0.as_str(), "member");
    let _ = client.marker;
    let _ = client.fallible;
}

#[test]
fn unconfigured_fallible_endpoint_keeps_its_configuration_location() {
    let line = line!() + 1;
    let client = MixedClient::<LocalInput>::test_value();
    let panic = catch_unwind(AssertUnwindSafe(|| (client.fallible)(String::new())))
        .expect_err("unconfigured fallible endpoint must not return a result");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("MixedClient"), "{message}");
    assert!(message.contains(".fallible"), "{message}");
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
}

#[test]
fn unconfigured_endpoint_reports_test_configuration_call_site() {
    let line = line!() + 1;
    let client = Client::test_value();
    let panic = catch_unwind(AssertUnwindSafe(|| (client.load)(String::new(), 0)))
        .expect_err("unconfigured endpoint");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
}

/// Records when a typed error leaves synchronous, pending or abandoned work.
struct OwnedError {
    /// Distinguishes the independently exercised success and failure routes.
    value: String,
    /// Counts destruction across the callback's owned values.
    drops: Arc<AtomicUsize>,
}

impl Drop for OwnedError {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Preserves the concrete error type through an owned asynchronous result.
type FallibleResponse = Pin<Box<dyn Future<Output = Result<String, OwnedError>> + Send>>;

/// Exercises a stored member alongside synchronous and asynchronous fallible callbacks.
#[derive(DependencyClient)]
struct AsyncClient {
    /// Checks that callback overrides leave explicit member defaults intact.
    #[dependency(member = String::from("configured"))]
    label: String,
    /// Returns ownership of its input error directly to the caller.
    #[dependency(test = Arc::new(Err))]
    sync: Arc<dyn Fn(OwnedError) -> Result<String, OwnedError> + Send + Sync>,
    /// Is overridden with a manually polled, caller-controlled future.
    load: Arc<dyn Fn(OwnedError) -> FallibleResponse + Send + Sync>,
}

#[test]
fn unconfigured_async_endpoint_drops_its_argument_before_any_future_is_polled() {
    let drops = Arc::new(AtomicUsize::new(0));
    let line = line!() + 1;
    let client = AsyncClient::test_value();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        (client.load)(OwnedError {
            value: String::from("unconfigured"),
            drops: Arc::clone(&drops),
        })
    }))
    .err()
    .expect("unconfigured async endpoint fails at invocation");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("AsyncClient.load"), "{message}");
    assert!(
        message.contains(&format!("{}:{line}:", file!())),
        "{message}"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(client);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn mixed_routes_preserve_typed_errors_and_pending_future_ownership() {
    let ready = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&ready);
    let client = AsyncClient {
        load: Arc::new(move |error| {
            let gate = Arc::clone(&gate);
            let mut error = Some(error);
            Box::pin(std::future::poll_fn(move |_| {
                if gate.load(Ordering::SeqCst) {
                    let error = error.take().unwrap();
                    Poll::Ready(if error.value == "success" {
                        Ok(error.value.clone())
                    } else {
                        Err(error)
                    })
                } else {
                    Poll::Pending
                }
            }))
        }),
        ..AsyncClient::test_value()
    };
    assert_eq!(client.label, "configured");
    let drops = Arc::new(AtomicUsize::new(0));
    let error = |value: &str| OwnedError {
        value: value.to_owned(),
        drops: Arc::clone(&drops),
    };
    let failure = (client.sync)(error("sync")).err().unwrap();
    assert_eq!(failure.value, "sync");
    drop(failure);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let mut pending = (client.load)(error("failure"));
    let mut abandoned = (client.load)(error("abandoned"));
    let mut success = (client.load)(error("success"));
    drop(client);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(pending.as_mut().poll(&mut cx).is_pending());
    assert!(abandoned.as_mut().poll(&mut cx).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(abandoned);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    ready.store(true, Ordering::SeqCst);
    let Poll::Ready(Err(failure)) = pending.as_mut().poll(&mut cx) else {
        panic!("typed error must emerge unchanged");
    };
    assert_eq!(failure.value, "failure");
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(failure);
    let Poll::Ready(Ok(value)) = success.as_mut().poll(&mut cx) else {
        panic!("success must emerge");
    };
    assert_eq!(value, "success");
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

#[test]
fn explicit_initialiser_panic_drops_earlier_members_without_starting_later_fields() {
    // The generated constructor takes no arguments. This trace belongs to this test thread only.
    thread_local! {
        static EVENTS: std::cell::RefCell<Vec<&'static str>> = const {
            std::cell::RefCell::new(Vec::new())
        };
    }

    /// Records disposal of the first field during construction or normal drop.
    struct FirstMember(usize);

    impl Drop for FirstMember {
        fn drop(&mut self) {
            EVENTS.with_borrow_mut(|events| events.push("first drop"));
        }
    }

    /// Uses one generated constructor for failure and successful field evaluation.
    #[derive(DependencyClient)]
    struct ConstructionClient<const FAIL: bool> {
        /// Owns a value before the next field can panic.
        #[dependency(member = {
            EVENTS.with_borrow_mut(|events| events.push("first init"));
            FirstMember(17)
        })]
        first: FirstMember,
        /// Interrupts construction only in the failure control.
        #[dependency(member = {
            EVENTS.with_borrow_mut(|events| events.push("middle init"));
            assert!(!FAIL, "dependency initialiser fault");
            29
        })]
        middle: usize,
        /// Must not start until both earlier fields finish construction.
        #[dependency(test = {
            EVENTS.with_borrow_mut(|events| events.push("last init"));
            Arc::new(|| {
                EVENTS.with_borrow_mut(|events| events.push("callback"));
                43
            })
        })]
        last: Arc<dyn Fn() -> usize + Send + Sync>,
    }

    let panic = catch_unwind(ConstructionClient::<true>::test_value)
        .err()
        .expect("the middle initialiser must stop construction");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"dependency initialiser fault")
    );
    assert_eq!(EVENTS.take(), ["first init", "middle init", "first drop"]);

    let client = ConstructionClient::<false>::test_value();
    assert_eq!(client.first.0, 17);
    assert_eq!(client.middle, 29);
    assert_eq!(
        EVENTS.with_borrow(Clone::clone),
        ["first init", "middle init", "last init"]
    );
    assert_eq!((client.last)(), 43);
    drop(client);
    assert_eq!(
        EVENTS.take(),
        [
            "first init",
            "middle init",
            "last init",
            "callback",
            "first drop",
        ]
    );
}
