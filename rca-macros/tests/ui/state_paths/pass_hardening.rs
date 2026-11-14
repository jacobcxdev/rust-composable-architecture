use rca_macros::{ActionPaths, CaseDescriptors, StatePaths};

struct Child;

#[derive(StatePaths)]
struct Recursive {
    child: Box<Self>,
}

#[derive(StatePaths)]
struct Defaults<T = Child, const N: usize = 3> {
    child: T,
    bytes: [u8; N],
}

#[derive(StatePaths)]
enum Cases {
    Tuple(Child),
    Named {
        child: Child,
    },
    #[expect(
        non_camel_case_types,
        reason = "the fixture verifies edition-2024 raw identifier preservation"
    )]
    r#gen(Child),
}

#[derive(StatePaths)]
enum RecursiveCases {
    Tuple(Box<Self>),
    Named { child: Box<Self> },
}

#[derive(StatePaths)]
enum OrdinaryKeyword {
    Gen(Child),
}

mod shadowed_core_names {
    use rca_macros::StatePaths;

    pub enum Option<T> {
        Value(T),
    }

    pub struct Child;

    #[derive(StatePaths)]
    pub struct State {
        #[rca(optional)]
        pub child: ::core::option::Option<Child>,
    }

    pub fn paths_compile() {
        let _ = StatePaths::child();
    }
}

type DefaultsNamespace = DefaultsPaths;

#[derive(StatePaths)]
struct CallbackState<F>
where
    F: for<'__rca_1> Fn(&'__rca_1 str),
{
    callback: for<'__rca> fn(&'__rca str),
    generic: F,
}

#[derive(ActionPaths)]
enum CallbackAction {
    Call(for<'__rca> fn(&'__rca str)),
}

#[derive(CaseDescriptors)]
enum CallbackEvent<F>
where
    F: for<'__rca_1> Fn(&'__rca_1 str),
{
    Call {
        callback: for<'__rca> fn(&'__rca str),
        generic: F,
    },
}

fn original(text: &str) {
    assert_eq!(text, "original");
}

fn replacement(text: &str) {
    assert_eq!(text, "replacement");
}

fn callback_lifetimes() {
    let original_text = String::from("original");
    let replacement_text = String::from("replacement");
    let mut state = CallbackState {
        callback: original,
        generic: original,
    };
    let path = CallbackStatePaths::callback();
    assert!(std::ptr::eq(path.get(&state), &state.callback));
    path.get(&state)(&original_text);
    *path.get_mut(&mut state) = replacement;
    (state.callback)(&replacement_text);
    CallbackStatePaths::generic().get(&state)(&original_text);

    let action_path = CallbackActionPaths::call();
    let action = action_path.embed(original);
    action_path.extract(&action).unwrap()(&original_text);

    let event = CallbackEvent::Call {
        callback: replacement,
        generic: original,
    };
    let CallbackEventCaseRef::Call { callback, generic } = CallbackEventCases::project(&event);
    callback(&replacement_text);
    generic(&original_text);
    assert_eq!(CallbackEventCases::case(&event), CallbackEventCase::Call);
    assert_eq!(
        CallbackEventCase::Call.label_with(|_| Some(original_text.as_str())),
        Some("original")
    );
}

fn main() {
    let _ = RecursivePaths::child();
    let _ = CasesPaths::tuple();
    let _ = CasesPaths::child();
    let _ = CasesPaths::r#gen();
    let _ = RecursiveCasesPaths::tuple();
    let _ = RecursiveCasesPaths::child();
    let _ = OrdinaryKeywordPaths::r#gen();
    shadowed_core_names::paths_compile();
    let _: Option<DefaultsNamespace> = None;
    callback_lifetimes();
}
