// Checks recursive types, defaults, raw names and private payloads.

use rca::KeyedAction;
use rca_macros::ActionPaths;

struct Child;

struct Row;

#[derive(ActionPaths)]
enum Recursive {
    Next(Box<Self>),
}

#[derive(ActionPaths)]
enum Defaults<Id, T = Child, const N: usize = 3> {
    Tuple(T),
    Named {
        value: [u8; N],
    },
    #[rca(keyed)]
    Row(KeyedAction<Id, Row>),
    Unit,
    #[rca(skip)]
    Multiple(Child, Child),
    #[expect(
        non_camel_case_types,
        reason = "the fixture verifies raw identifier preservation"
    )]
    r#gen(Child),
    Union(Child),
    MacroRules(Child),
}

#[derive(ActionPaths)]
enum Bounded<T>
where
    T: ?Sized,
{
    Child(Box<T>),
}

mod nested {
    use super::{ActionPaths, Child};

    #[derive(ActionPaths)]
    pub enum PublicAction {
        Child(Child),
        SetName { name: Child },
    }

    pub fn paths_compile() {
        let _ = PublicActionPaths::child();
        let _ = PublicActionPaths::set_name();
    }
}

fn main() {
    let _ = RecursivePaths::next();
    let _ = DefaultsPaths::<u8>::tuple();
    let _ = DefaultsPaths::<u8>::named();
    let _ = DefaultsPaths::<u8>::row();
    let _ = DefaultsPaths::<u8>::r#gen();
    let _ = DefaultsPaths::<u8>::union();
    let _ = DefaultsPaths::<u8>::macro_rules();
    let _ = BoundedPaths::<str>::child();
    nested::paths_compile();
}
