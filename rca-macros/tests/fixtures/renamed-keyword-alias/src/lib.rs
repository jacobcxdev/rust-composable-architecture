//! Checks state paths when the dependency alias is a Rust keyword.

use r#gen::StatePaths;

struct Child(u8);

#[derive(StatePaths)]
struct State {
    child: Child,
}

fn paths_compile() {
    let _ = StatePaths::child();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_alias_path_mutates_the_original_child() {
        paths_compile();
        let path = StatePaths::child();
        let mut state = State { child: Child(7) };
        assert_eq!(path.get(&state).0, 7);
        path.get_mut(&mut state).0 = 11;
        assert_eq!(state.child.0, 11);
    }
}
