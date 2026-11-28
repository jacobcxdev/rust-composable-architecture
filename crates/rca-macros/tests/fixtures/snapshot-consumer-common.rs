// Shared source for consumers that vary dependency names and the extra feature.

/// Supplies captured fields selected by the consuming package's feature configuration.
#[derive(Snapshot)]
pub struct Source {
    /// Remains present in both feature configurations.
    pub value: usize,
    /// Exercises owned text when the extra feature is enabled.
    #[cfg(feature = "extra")]
    pub extra: String,
    /// Exercises the alternative field without the extra feature.
    #[cfg(not(feature = "extra"))]
    pub base: u8,
    #[cfg(any())]
    missing: Missing,
}

/// Exercises generated cases and fields across the same feature boundary.
#[derive(Snapshot)]
pub enum Event {
    /// Remains present in both feature configurations.
    Value(usize),
    /// Adds a named payload when the extra feature is enabled.
    #[cfg(feature = "extra")]
    Extra {
        /// Text copied into the generated snapshot.
        field: String,
    },
    /// Exercises conditional attributes rather than a direct feature predicate.
    #[cfg_attr(not(feature = "extra"), cfg(any()))]
    Guarded(String),
}

/// Captures through the trait imported by the consuming package.
pub fn capture(source: &Source) -> <Source as Snapshot>::Value {
    source.snapshot()
}

#[test]
fn captures_work_through_this_dependency_boundary() {
    let source = Source {
        value: 7,
        #[cfg(feature = "extra")]
        extra: "extra".into(),
        #[cfg(not(feature = "extra"))]
        base: 3,
    };
    let captured = capture(&source);
    assert_eq!(captured.value, 7);
    assert_eq!(captured, captured.clone());
    assert_eq!(Event::Value(4).snapshot(), EventSnapshot::Value(4));
    #[cfg(feature = "extra")]
    {
        assert_eq!(captured.extra, "extra");
        assert_eq!(
            Event::Extra { field: "x".into() }.snapshot(),
            EventSnapshot::Extra { field: "x".into() }
        );
        assert_eq!(
            Event::Guarded("y".into()).snapshot(),
            EventSnapshot::Guarded("y".into())
        );
    }
    #[cfg(not(feature = "extra"))]
    assert_eq!(captured.base, 3);
}
