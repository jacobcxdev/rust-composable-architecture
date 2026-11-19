//! Checks state paths when generated references must target the current crate.

pub use rca_core::{CaseStatePath, KeyedState, KeyedStatePath, OptionalStatePath, StatePath};

use rca_macros::StatePaths;

pub struct Child(pub u8);

#[derive(StatePaths)]
pub struct State {
    pub child: Child,
}

pub fn paths_compile() {
    let _ = StatePaths::child();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_self_path_mutates_the_original_child() {
        paths_compile();
        let path = StatePaths::child();
        let mut state = State { child: Child(7) };
        assert_eq!(path.get(&state).0, 7);
        path.get_mut(&mut state).0 = 11;
        assert_eq!(state.child.0, 11);
    }
}
