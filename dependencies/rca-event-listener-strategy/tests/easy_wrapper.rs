//! Macro grammar checks and execution of default and explicit type arguments.
//!
//! Phantom fixtures check generated types without being polled. Borrowed,
//! unsized output is executed separately in `public_contracts.rs`.

#![allow(clippy::multiple_bound_locations)]

use event_listener_strategy::{easy_wrapper, EventListenerFuture, Strategy};
use std::{marker::PhantomData, pin::Pin, task::Poll};

#[cfg(target_family = "wasm")]
use wasm_bindgen_test::wasm_bindgen_test as test;

#[test]
fn easy_wrapper_generics() {
    /// A ready future for the wrapper without generic parameters.
    struct MyStrategy;

    impl EventListenerFuture for MyStrategy {
        type Output = ();

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            Poll::Ready(())
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper(MyStrategy => ());
        #[cfg(all(feature = "std", not(target_family = "wasm")))]
        wait();
    }

    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    MyEasyWrapper::_new(MyStrategy).wait();

    /// A type-parameter fixture used only to compile the generated wrapper.
    struct MyStrategy2<T> {
        /// Retains the output type without manufacturing a value to return.
        _marker: PhantomData<T>,
    }

    impl<T> EventListenerFuture for MyStrategy2<T> {
        type Output = T;

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper2<T>(MyStrategy2<T> => T);
        #[cfg(all(feature = "std", not(target_family = "wasm")))]
        wait();
    }

    /// A lifetime-parameter fixture whose generated wrapper is not executed.
    struct MyStrategylt<'a> {
        /// Carries the output lifetime through the generated type.
        _marker: PhantomData<&'a ()>,
    }

    impl<'a> EventListenerFuture for MyStrategylt<'a> {
        type Output = &'a ();

        fn poll_with_strategy<'b, S: Strategy<'b>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapperlt<'a>(MyStrategylt<'a> => &'a ());
        #[cfg(all(feature = "std", not(target_family = "wasm")))]
        wait();
    }

    /// Checks unsized parameters with a separate lifetime bound.
    struct MyStrategy3<'a, T: ?Sized>
    where
        T: 'a,
    {
        /// Carries both parameters without requiring an owned `T`.
        _marker: PhantomData<&'a T>,
    }

    impl<'a, T: ?Sized> EventListenerFuture for MyStrategy3<'a, T>
    where
        T: 'a,
    {
        type Output = &'a T;

        fn poll_with_strategy<'b, S: Strategy<'b>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            unreachable!()
        }
    }

    easy_wrapper! {
        struct MyEasyWrapper3<'a, T: ?Sized>(MyStrategy3<'a, T> => &'a T) where T: 'a;
        #[cfg(all(feature = "std", not(target_family = "wasm")))]
        wait();
    }
}

#[test]
fn easy_wrapper_default_type_parameter_is_declaration_only() {
    /// Returns the supplied value so both generated type choices are exercised.
    struct Value<T>(T);

    impl<T: Copy> EventListenerFuture for Value<T> {
        type Output = T;

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            Poll::Ready(self.0)
        }
    }

    easy_wrapper! {
        struct DefaultWrapper<T: Copy = u8>(Value<T> => T);
        #[cfg(all(feature = "std", not(target_family = "wasm")))]
        wait();
    }

    let default: DefaultWrapper = DefaultWrapper::_new(Value(7));
    assert_eq!(futures_lite::future::block_on(default), 7u8);
    let explicit = DefaultWrapper::<u16>::_new(Value(300));
    assert_eq!(futures_lite::future::block_on(explicit), 300);
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    assert_eq!(DefaultWrapper::<u16>::_new(Value(301)).wait(), 301);
}
