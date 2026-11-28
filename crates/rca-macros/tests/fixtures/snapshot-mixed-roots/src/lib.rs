//! Checks which trait a macro implements when two possible roots appear in one manifest.

#[cfg(all(test, feature = "short"))]
mod short {
    /// Derives through the real facade, which snapshots a byte as another byte.
    #[derive(short_arch::Snapshot)]
    struct Source(u8);

    #[test]
    fn facade_uses_its_own_snapshot_trait() {
        let captured = <Source as short_arch::Snapshot>::snapshot(&Source(7));
        let byte: u8 = captured.0;
        assert_eq!(byte, 7);
    }
}

#[cfg(all(test, feature = "core"))]
mod core_origin {
    /// Imports the standalone macro for a different core with its own trait.
    #[derive(rca_macros::Snapshot)]
    struct Source(u8);

    #[test]
    fn standalone_macro_uses_the_direct_core() {
        let captured = <Source as core_arch::Snapshot>::snapshot(&Source(9));
        let bytes: [u8; 1] = captured.0;
        assert_eq!(bytes, [9]);
    }
}
