//! Checks namespace hygiene and trait bounds without requiring domain traits.

use rca_macros::{ActionPaths, StatePaths};

/// Deliberately lacks Debug so namespace formatting cannot borrow that bound.
struct NotDebug;

/// Requires namespace formatting to propagate the first writer failure.
fn assert_debug_error(value: &impl std::fmt::Debug) {
    /// Counts writes while refusing every string.
    struct Reject(usize);
    impl std::fmt::Write for Reject {
        fn write_str(&mut self, _: &str) -> std::fmt::Result {
            self.0 += 1;
            Err(std::fmt::Error)
        }
    }
    let mut writer = Reject(0);
    assert!(std::fmt::write(&mut writer, format_args!("{value:?}")).is_err());
    assert_eq!(writer.0, 1);
}

#[test]
#[expect(
    non_camel_case_types,
    reason = "generated helper name collision regression"
)]
fn generic_parameters_cannot_shadow_block_local_namespace_helpers() {
    #[derive(StatePaths)]
    struct Local<__rca_path_namespace_LocalPaths> {
        value: __rca_path_namespace_LocalPaths,
    }

    #[derive(ActionPaths)]
    enum Event<r#__rca_path_namespace_EventPaths> {
        Value(r#__rca_path_namespace_EventPaths),
    }

    let mut state = Local { value: 42 };
    assert_eq!(*LocalPaths::value().get(&state), 42);
    *LocalPaths::value().get_mut(&mut state) = 43;
    assert_eq!(*LocalPaths::value().get(&state), 43);
    let action = EventPaths::value().embed(42);
    assert_eq!(EventPaths::value().extract(&action), Some(&42));
}

/// Exercises type, lifetime and const parameters beside colliding caller names.
#[expect(non_upper_case_globals, reason = "formatter hygiene regression")]
mod generic {
    use super::{ActionPaths, NotDebug, StatePaths};
    use std::rc::Rc;

    /// Lets a macro supply the const parameter's type.
    type Count = usize;
    /// Expands in type position without changing the caller's alias.
    macro_rules! count_type {
        () => {
            Count
        };
    }

    /// Checks that shorter-lived input types do not restrict the namespace lifetime.
    #[derive(StatePaths)]
    struct Contravariant<T, const N: count_type!() = 3> {
        marker: std::marker::PhantomData<fn(T)>,
        bytes: [u8; N],
    }

    /// Supplies a compiler check for the namespace's declared variance.
    fn contravariant(namespace: ContravariantPaths<&str>) -> ContravariantPaths<&'static str> {
        namespace
    }

    /// Deliberately collides with a formatter parameter's conventional name.
    const formatter: usize = 3;
    /// Keeps the caller's constant hidden inside a type-position macro.
    macro_rules! hidden_length {
        () => {
            [u8; formatter]
        };
    }

    /// Remains usable from a sibling module with defaulted generic parameters.
    #[derive(StatePaths)]
    pub struct Borrowed<'a, T: ?Sized = str, const N: usize = 3>
    where
        hidden_length!(): Sized,
    {
        /// Borrows the caller's possibly unsized payload.
        pub value: &'a T,
        /// Retains the declared const-generic array length.
        pub bytes: [u8; N],
    }

    /// Uses a colliding const name alongside a borrowed, possibly unsized payload.
    #[derive(ActionPaths)]
    pub enum Event<'a, T: ?Sized, const r#formatter: usize> {
        /// Retains the exact borrowed input type.
        Value(&'a T),
        /// Retains the caller's raw const parameter.
        Bytes([u8; r#formatter]),
    }

    /// Checks namespace auto traits without constructing a domain value.
    fn assert_traits<T: Send + Sync>() {}

    #[test]
    fn generic_namespaces_are_type_only_without_domain_bounds() {
        assert_traits::<BorrowedPaths<'_, Rc<NotDebug>>>();
        assert_traits::<EventPaths<'_, Rc<NotDebug>, 3>>();
        assert_traits::<BorrowedPaths<'_>>();
        assert_traits::<ContravariantPaths<Rc<NotDebug>>>();
        let _ = contravariant;
        let mut state = Contravariant::<Rc<NotDebug>> {
            marker: std::marker::PhantomData,
            bytes: [5; 3],
        };
        let _ = ContravariantPaths::marker().get(&state);
        assert_eq!(ContravariantPaths::bytes().get(&state), &[5; 3]);
        let marker = std::ptr::addr_of!(state.marker);
        assert!(std::ptr::eq(
            ContravariantPaths::marker().get_mut(&mut state),
            marker
        ));
        ContravariantPaths::bytes().get_mut(&mut state)[0] = 6;
        assert_eq!(ContravariantPaths::bytes().get(&state), &[6, 5, 5]);
        let value = Rc::new(NotDebug);
        let mut state = Borrowed {
            value: &value,
            bytes: [1; 3],
        };
        assert!(Rc::ptr_eq(BorrowedPaths::value().get(&state), &value));
        assert_eq!(BorrowedPaths::bytes().get(&state), &[1; 3]);
        let replacement = Rc::new(NotDebug);
        *BorrowedPaths::value().get_mut(&mut state) = &replacement;
        BorrowedPaths::bytes().get_mut(&mut state)[0] = 2;
        assert!(Rc::ptr_eq(BorrowedPaths::value().get(&state), &replacement));
        assert_eq!(BorrowedPaths::bytes().get(&state), &[2, 1, 1]);
        let action = EventPaths::<Rc<NotDebug>, 3>::value().embed(&value);
        assert!(EventPaths::value().extract(&action).is_some());
        let bytes = EventPaths::<Rc<NotDebug>, 3>::bytes().embed([2; 3]);
        assert_eq!(EventPaths::bytes().extract(&bytes), Some(&[2; 3]));
        assert!(EventPaths::value().extract(&bytes).is_none());
        assert!(EventPaths::bytes().extract(&action).is_none());
        assert_ne!(
            std::any::TypeId::of::<BorrowedPaths<'static, str>>(),
            std::any::TypeId::of::<EventPaths<'static, str, 3>>()
        );
    }
}

#[test]
fn generic_namespace_aliases_remain_usable_outside_the_defining_module() {
    let mut state = generic::Borrowed {
        value: "borrowed",
        bytes: [4; 3],
    };
    assert_eq!(*generic::BorrowedPaths::value().get(&state), "borrowed");
    *generic::BorrowedPaths::value().get_mut(&mut state) = "changed";
    assert_eq!(*generic::BorrowedPaths::value().get(&state), "changed");
    let event = generic::EventPaths::<str, 3>::value().embed("event");
    assert_eq!(generic::EventPaths::value().extract(&event), Some(&"event"));
    let bytes = generic::EventPaths::<str, 3>::bytes().embed([5; 3]);
    assert_eq!(generic::EventPaths::bytes().extract(&bytes), Some(&[5; 3]));
    assert!(generic::EventPaths::value().extract(&bytes).is_none());
    assert!(generic::EventPaths::bytes().extract(&event).is_none());
}

/// Supplies a non-Debug field for unit namespace formatting.
#[derive(StatePaths)]
struct State {
    value: NotDebug,
}

/// Supplies a non-Debug state-case payload.
#[derive(StatePaths)]
enum Destination {
    Value(NotDebug),
}

/// Supplies a non-Debug action payload.
#[derive(ActionPaths)]
enum Action {
    Value(NotDebug),
}

#[test]
fn unit_namespaces_format_without_domain_debug() {
    assert_eq!(format!("{StatePaths:?}"), "StatePaths");
    assert_eq!(format!("{DestinationPaths:?}"), "DestinationPaths");
    assert_eq!(format!("{ActionPaths:?}"), "ActionPaths");
    assert_debug_error(&StatePaths);
    assert_debug_error(&DestinationPaths);
    assert_debug_error(&ActionPaths);

    let mut state = State { value: NotDebug };
    let _ = StatePaths::value().get(&state);
    let value = std::ptr::addr_of!(state.value);
    assert!(std::ptr::eq(StatePaths::value().get_mut(&mut state), value));
    let mut destination = Destination::Value(NotDebug);
    let _ = DestinationPaths::value().get(&destination);
    let Destination::Value(value) = &destination;
    let value = std::ptr::from_ref(value);
    assert!(std::ptr::eq(
        DestinationPaths::value().get_mut(&mut destination).unwrap(),
        value
    ));
    let action = ActionPaths::value().embed(NotDebug);
    assert!(ActionPaths::value().extract(&action).is_some());
}

/// Checks constructible lifetime/const namespaces and colliding caller constants.
#[expect(
    non_upper_case_globals,
    reason = "hostile caller constants and generated const parameters"
)]
mod borrowed_const {
    use super::{ActionPaths, NotDebug, StatePaths};
    use std::marker::PhantomData;
    use std::rc::Rc;

    /// Must keep referring to this constant inside the caller's type macro.
    const formatter: usize = 3;

    /// Uses the colliding constant in an array type.
    macro_rules! hidden_length {
        () => {
            [u8; formatter]
        };
    }

    /// Borrows a non-Send, non-Debug domain without imposing its traits on paths.
    #[derive(StatePaths)]
    struct Borrowed<'a, const N: usize>
    where
        hidden_length!(): Sized,
    {
        value: &'a Rc<NotDebug>,
        bytes: [u8; N],
    }

    /// Combines a borrowed local payload with a raw const parameter.
    #[derive(ActionPaths)]
    enum Event<'a, const r#formatter: usize> {
        Value(&'a Rc<NotDebug>),
        Bytes([u8; r#formatter]),
    }

    /// Supplies a namespace with only a lifetime parameter.
    #[derive(StatePaths)]
    struct Text<'a> {
        value: &'a str,
    }

    /// Supplies a namespace with only a const parameter.
    #[derive(StatePaths)]
    struct Array<const N: usize> {
        bytes: [NotDebug; N],
    }

    /// Checks namespace traits without requiring those traits on its domain.
    fn assert_traits<T: std::fmt::Debug + Send + Sync>() {}

    #[test]
    fn namespaces_format_without_domain_bounds_and_preserve_construction() {
        assert_traits::<BorrowedPaths<'_, 3>>();
        assert_traits::<EventPaths<'_, 3>>();
        assert_traits::<TextPaths<'_>>();
        assert_traits::<ArrayPaths<1>>();
        let state = BorrowedPaths::<'_, 3>(PhantomData);
        let action = EventPaths::<'_, 3>(PhantomData);
        let BorrowedPaths(_) = state;
        let EventPaths(_) = action;
        assert!(format!("{state:?}").starts_with("BorrowedPaths("));
        assert!(format!("{action:?}").starts_with("EventPaths("));
        super::assert_debug_error(&state);
        super::assert_debug_error(&action);
        let text_namespace = TextPaths(PhantomData);
        let array_namespace = ArrayPaths::<1>(PhantomData);
        assert!(format!("{text_namespace:?}").starts_with("TextPaths("));
        assert!(format!("{array_namespace:?}").starts_with("ArrayPaths("));
        super::assert_debug_error(&text_namespace);
        super::assert_debug_error(&array_namespace);
        let value = Rc::new(NotDebug);
        let mut domain = Borrowed {
            value: &value,
            bytes: [0; 3],
        };
        assert_eq!(BorrowedPaths::bytes().get(&domain), &[0; 3]);
        assert!(Rc::ptr_eq(BorrowedPaths::value().get(&domain), &value));
        let replacement = Rc::new(NotDebug);
        *BorrowedPaths::value().get_mut(&mut domain) = &replacement;
        BorrowedPaths::bytes().get_mut(&mut domain)[0] = 2;
        assert!(Rc::ptr_eq(
            BorrowedPaths::value().get(&domain),
            &replacement
        ));
        assert_eq!(BorrowedPaths::bytes().get(&domain), &[2, 0, 0]);
        let event = EventPaths::<3>::value().embed(&value);
        assert!(EventPaths::value().extract(&event).is_some());
        let bytes = EventPaths::<'_, 3>::bytes().embed([1; 3]);
        assert_eq!(EventPaths::bytes().extract(&bytes), Some(&[1; 3]));
        assert!(EventPaths::value().extract(&bytes).is_none());
        assert!(EventPaths::bytes().extract(&event).is_none());
        let mut text = Text { value: "borrowed" };
        assert_eq!(*TextPaths::value().get(&text), "borrowed");
        *TextPaths::value().get_mut(&mut text) = "changed";
        assert_eq!(*TextPaths::value().get(&text), "changed");
        let mut array = Array { bytes: [NotDebug] };
        assert_eq!(ArrayPaths::bytes().get(&array).len(), 1);
        let bytes = std::ptr::addr_of!(array.bytes);
        assert!(std::ptr::eq(ArrayPaths::bytes().get_mut(&mut array), bytes));
    }
}
