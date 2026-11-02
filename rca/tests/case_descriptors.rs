//! Checks owned case embedding and borrowed macro projections through the façade.

#[test]
fn facade_owned_embedding_checks_root_and_preserves_optional_none() {
    enum Event {
        Value(Option<u16>),
    }
    let path = rca::OwnedCasePath::optional(Event::Value);
    let event = path.try_embed::<Event>(Box::new(None::<u16>)).unwrap();
    assert!(matches!(event, Event::Value(None)));
    let rejected: rca::OwnedCaseMismatch = path.try_embed::<String>(Box::new(7_u16)).err().unwrap();
    assert!(rejected.is_root_mismatch());
    assert_eq!(*rejected.into_input().downcast::<u16>().unwrap(), 7);
    assert!(matches!(
        path.try_embed::<Event>(Box::new(9_u16)).unwrap(),
        Event::Value(Some(9))
    ));
}

#[cfg(feature = "macros")]
#[test]
fn facade_catalogue_borrows_local_fields_without_debug_or_clone() {
    /// Keeps the projection local and exposes accidental payload cloning.
    struct LocalPayload(std::rc::Rc<String>);

    #[derive(rca::CaseDescriptors)]
    enum Event<'a> {
        Idle,
        Pair(LocalPayload, &'a str),
    }

    let text = String::from("borrowed");
    let event = Event::Pair(LocalPayload(std::rc::Rc::new(String::from("owned"))), &text);
    assert_eq!(EventCases::ALL, &[EventCase::Idle, EventCase::Pair]);
    let kind = EventCases::case(&event);
    assert_eq!(kind, EventCase::Pair);
    assert_eq!(kind.label_with(|_| None), None);
    let EventCaseRef::Pair(payload, label) = EventCases::project(&event) else {
        panic!("pair expected")
    };
    let Event::Pair(original, _) = &event else {
        panic!("pair expected")
    };
    assert!(std::ptr::eq(payload, original));
    assert_eq!(payload.0.as_str(), "owned");
    assert_eq!(*label, "borrowed");
    assert_eq!(std::rc::Rc::strong_count(&payload.0), 1);
    assert!(matches!(
        EventCases::project(&Event::Idle),
        EventCaseRef::Idle
    ));
}
