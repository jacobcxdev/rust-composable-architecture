//! Checks action paths when the dependency alias is a Rust keyword.

use r#gen::ActionPaths;

struct Child(u8);

#[derive(ActionPaths)]
enum Action {
    Child(Child),
}

pub fn paths_compile() {
    let _ = ActionPaths::child();
    let _ = r#gen::reducers![()];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_alias_path_preserves_the_child_payload() {
        paths_compile();
        let path = ActionPaths::child();
        let action = path.embed(Child(7));
        assert_eq!(path.extract(&action).unwrap().0, 7);
        assert!(matches!(action, Action::Child(Child(7))));
    }
}
