use std::cell::Cell;
use std::rc::Rc;

use rca_core::OwnedCasePath;

struct MoveOnly(Box<u8>);

enum Root {
    Value(MoveOnly),
    Optional(Option<MoveOnly>),
}

#[test]
fn exact_embedding_moves_the_payload_into_the_declared_root() {
    let path = OwnedCasePath::new(Root::Value);
    let value = MoveOnly(Box::new(7));
    let address = &raw const *value.0;
    let root = path.try_embed::<Root>(Box::new(value)).unwrap();
    let Root::Value(value) = root else {
        panic!("wrong case")
    };
    assert_eq!(*value.0, 7);
    assert_eq!(&raw const *value.0, address);
}

#[test]
fn optional_embedding_accepts_exact_none_and_some_before_bare_promotion() {
    let path = OwnedCasePath::optional(Root::Optional);
    assert!(matches!(
        path.try_embed::<Root>(Box::new(None::<MoveOnly>)).unwrap(),
        Root::Optional(None)
    ));
    let root = path
        .try_embed::<Root>(Box::new(Some(MoveOnly(Box::new(8)))))
        .unwrap();
    assert!(matches!(root, Root::Optional(Some(value)) if *value.0 == 8));
    let root = path
        .try_embed::<Root>(Box::new(MoveOnly(Box::new(9))))
        .unwrap();
    assert!(matches!(root, Root::Optional(Some(value)) if *value.0 == 9));
}

#[test]
fn exact_optional_embedding_does_not_implicitly_promote() {
    let path = OwnedCasePath::new(Root::Optional);
    let rejected = path
        .try_embed::<Root>(Box::new(MoveOnly(Box::new(3))))
        .err()
        .unwrap();
    assert!(rejected.is_payload_mismatch());
    assert_eq!(
        format!("{rejected:?}"),
        format!(
            "OwnedCaseMismatch {{ root_mismatch: false, backtrace: {:?}, .. }}",
            rejected.backtrace()
        )
    );
    assert_eq!(
        rejected.to_string(),
        format!(
            "owned payload type does not match the case path\n{}",
            rejected.backtrace()
        )
    );
    let value = rejected.into_input().downcast::<MoveOnly>().unwrap();
    assert_eq!(*value.0, 3);
}

#[test]
fn wrong_root_rejects_before_embedding_and_preserves_owned_input() {
    struct RejectingWriter(usize);
    impl std::fmt::Write for RejectingWriter {
        fn write_str(&mut self, _: &str) -> std::fmt::Result {
            self.0 += 1;
            Err(std::fmt::Error)
        }
    }

    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);
    let path = OwnedCasePath::new(move |value| {
        observed.set(observed.get() + 1);
        Root::Value(value)
    });
    assert_eq!(
        format!("{path:?}"),
        format!(
            "OwnedCasePath {{ root: {:?}, .. }}",
            std::any::TypeId::of::<Root>()
        )
    );
    let value = MoveOnly(Box::new(4));
    let address = &raw const *value.0;
    let rejected = path
        .try_embed::<Option<MoveOnly>>(Box::new(value))
        .err()
        .unwrap();
    assert!(rejected.is_root_mismatch());
    assert_eq!(
        format!("{rejected:?}"),
        format!(
            "OwnedCaseMismatch {{ root_mismatch: true, backtrace: {:?}, .. }}",
            rejected.backtrace()
        )
    );
    assert_eq!(
        rejected.to_string(),
        format!(
            "requested root type does not match the owned case path\n{}",
            rejected.backtrace()
        )
    );
    let mut writer = RejectingWriter(0);
    assert_eq!(
        std::fmt::Write::write_fmt(&mut writer, format_args!("{rejected}")),
        Err(std::fmt::Error)
    );
    assert_eq!(writer.0, 1);
    assert_eq!(calls.get(), 0);
    let value = rejected.into_input().downcast::<MoveOnly>().unwrap();
    assert_eq!(&raw const *value.0, address);
}

#[test]
fn incompatible_optional_payload_is_returned_without_coercion_or_early_drop() {
    struct Rejected(Rc<Cell<usize>>);
    impl Drop for Rejected {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let path = OwnedCasePath::optional(Root::Optional);
    let rejected = path
        .try_embed::<Root>(Box::new(Rejected(Rc::clone(&drops))))
        .err()
        .unwrap();
    assert!(rejected.is_payload_mismatch());
    let _ = rejected.backtrace().status();
    assert_eq!(drops.get(), 0);
    let input = rejected.into_input().downcast::<Rejected>().unwrap();
    drop(input);
    assert_eq!(drops.get(), 1);
}

#[test]
#[expect(
    clippy::option_option,
    reason = "nested options exercise exact-versus-promoted optional payloads"
)]
fn nested_optional_payloads_distinguish_exact_none_from_promoted_none() {
    enum Nested {
        Value(Option<Option<u8>>),
    }
    let path = OwnedCasePath::optional(Nested::Value);
    assert!(matches!(
        path.try_embed::<Nested>(Box::new(None::<Option<u8>>))
            .unwrap(),
        Nested::Value(None)
    ));
    assert!(matches!(
        path.try_embed::<Nested>(Box::new(Some(None::<u8>)))
            .unwrap(),
        Nested::Value(Some(None))
    ));
    assert!(matches!(
        path.try_embed::<Nested>(Box::new(None::<u8>)).unwrap(),
        Nested::Value(Some(None))
    ));
    assert!(matches!(
        path.try_embed::<Nested>(Box::new(Some(5_u8))).unwrap(),
        Nested::Value(Some(Some(5)))
    ));
}

#[test]
fn caller_embedding_supports_unit_and_owned_heterogeneous_named_payloads() {
    enum Aggregate {
        Unit,
        Named { value: MoveOnly, count: usize },
    }
    let unit = OwnedCasePath::new(|()| Aggregate::Unit);
    assert!(matches!(
        unit.try_embed::<Aggregate>(Box::new(())).unwrap(),
        Aggregate::Unit
    ));
    let named = OwnedCasePath::new(|(value, count)| Aggregate::Named { value, count });
    let root = named
        .try_embed::<Aggregate>(Box::new((MoveOnly(Box::new(5)), 7_usize)))
        .unwrap();
    assert!(matches!(root, Aggregate::Named { value, count: 7 } if *value.0 == 5));
}

#[test]
fn panicking_embedding_releases_the_transferred_payload_once() {
    struct Payload(Rc<Cell<usize>>);

    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let drops = Rc::new(Cell::new(0));
    let path = OwnedCasePath::new(|_payload: Payload| -> Root {
        panic!("embedding failure");
    });
    let payload = Box::new(Payload(Rc::clone(&drops)));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = path.try_embed::<Root>(payload);
    }))
    .expect_err("embedding panic propagates without replacing its payload");

    assert_eq!(failure.downcast_ref::<&str>(), Some(&"embedding failure"));
    assert_eq!(drops.get(), 1);
    drop(path);
    assert_eq!(drops.get(), 1);
}
