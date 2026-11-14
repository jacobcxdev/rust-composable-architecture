use core_api::Snapshot;

// A borrowed, non-Clone source uses both ordinary core and facade exports.
struct Borrowed<'a>(&'a str);
impl facade::Snapshot for Borrowed<'_> {
    type Value = String;
    fn snapshot(&self) -> String {
        self.0.to_owned()
    }
}

#[cfg(feature = "hosted")]
mod hosted {
    use core_api::{Effect, ReduceContext, Reducer, Snapshot};

    struct Source(usize);
    #[cfg_attr(not(feature = "no-clone"), derive(Clone))]
    #[cfg_attr(not(feature = "no-debug"), derive(Debug))]
    #[cfg_attr(not(feature = "no-equality"), derive(PartialEq))]
    struct Value {
        #[cfg(not(feature = "no-send"))]
        owned: usize,
        #[cfg(feature = "no-send")]
        owned: std::rc::Rc<usize>,
    }
    impl Snapshot for Source {
        type Value = Value;
        fn snapshot(&self) -> Value {
            Value {
                #[cfg(not(feature = "no-send"))]
                owned: self.0,
                #[cfg(feature = "no-send")]
                owned: std::rc::Rc::new(self.0),
            }
        }
    }

    struct Feature;
    impl Reducer for Feature {
        type State = Source;
        type Action = ();
        fn reduce(&mut self, _: &mut Source, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::none()
        }
    }

    pub fn check() {
        // This standalone capture remains valid for every feature combination.
        let _ = Source(7).snapshot();
        // Only this hosted consumer adds Clone/Debug/PartialEq/Send requirements.
        let _ = rca_test::TestStore::projected(Source(7), Feature, Snapshot::snapshot);
    }
}

fn main() {
    let history = {
        let local = String::from("owned");
        Borrowed(&local).snapshot()
    };
    assert_eq!(history, "owned");
    #[cfg(feature = "hosted")]
    hosted::check();
}
