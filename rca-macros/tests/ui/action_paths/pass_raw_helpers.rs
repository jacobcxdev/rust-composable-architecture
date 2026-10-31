#![deny(warnings)]

use rca::{KeyedAction, KeyedActionPath};
use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum r#Action {
    #[r#rca(r#keyed)]
    Row(KeyedAction<u8, String>),
    #[rca(r#skip)]
    Ignored,
}

fn main() {
    keyword_root::check();
    let path: KeyedActionPath<_, _, _, _, _> = ActionPaths::row();
    let action = path.embed(7, String::from("move only"));
    let Action::Row(original) = &action else {
        panic!("the keyed path must embed its selected case");
    };
    let (key, payload) = path.extract(&action).unwrap();
    assert_eq!((*key, payload.as_str()), (7, "move only"));
    assert!(std::ptr::eq(payload, original.action()));
    assert!(path.extract(&Action::Ignored).is_none());
}

#[allow(
    non_camel_case_types,
    reason = "the witness deliberately uses a Rust keyword"
)]
mod keyword_root {
    #[derive(rca_macros::ActionPaths)]
    enum r#type {
        Value(String),
    }

    pub fn check() {
        let path = typePaths::value();
        let value = path.embed(String::from("borrowed"));
        let r#type::Value(original) = &value;
        assert!(std::ptr::eq(path.extract(&value).unwrap(), original));
    }
}
