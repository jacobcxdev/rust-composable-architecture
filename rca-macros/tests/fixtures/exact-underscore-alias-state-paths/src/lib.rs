//! Checks state paths through the facade's exact underscore alias.

use rust_composable_architecture::{DependencyKey as _, StatePaths};

#[derive(rust_composable_architecture::DependencyKey)]
#[dependency(value = u8, live = 7)]
struct CountKey;

#[derive(StatePaths)]
struct State {
    child: u8,
}

pub fn derives_compile() {
    assert_eq!(CountKey::live_value(), 7);
    let _ = StatePaths::child();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_path_mutates_the_original_state() {
        derives_compile();
        let path = StatePaths::child();
        let mut state = State { child: 3 };
        assert_eq!(*path.get(&state), 3);
        *path.get_mut(&mut state) = 9;
        assert_eq!(state.child, 9);
    }
}
