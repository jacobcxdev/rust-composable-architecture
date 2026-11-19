use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use rca_core::{
    ActionPath, CaseStatePath, Effect, IfCase, IfCaseIdentified, IfSome, IfSomeIdentified,
    OptionalStatePath, Reduce, ReduceContext, Reducer, ReducerExt, ScopeIdentified, StatePath,
    if_case, if_some_identified, scope_identified,
};

struct Child<S> {
    id: u64,
    value: S,
}
enum Case<S> {
    Present(Child<S>),
    Absent,
}
struct State<S> {
    required: Child<S>,
    optional: Option<Child<S>>,
    case: Case<S>,
}

fn identity<S>(state: &Child<S>) -> &u64 {
    &state.id
}

fn captured_identity<C>(capture: &C) -> impl for<'a> Fn(&'a Child<()>) -> &'a u64 + '_ {
    move |state| {
        std::hint::black_box(capture);
        &state.id
    }
}

// Generic state has neither Send nor 'static; generic actions only retain
// 'static. All five adapters below type-check through Reducer::reduce.
fn domains<S, A: 'static>(state: &mut State<S>, action: &A, context: &mut ReduceContext<'_>) {
    fn send_sync<T: Send + Sync>(_: &T) {}
    let mut required = ScopeIdentified::new(
        Reduce::new(|_: &mut State<S>, _: &A, _| Effect::none()),
        StatePath::new(
            |s: &State<S>| &s.required,
            |s: &mut State<S>| &mut s.required,
        ),
        ActionPath::new(|a: &A| Some(a), |a| a),
        identity::<S>,
        Reduce::new(|_: &mut Child<S>, _: &A, _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut required, state, action, context);
    send_sync(&required);
    let mut optional = IfSome::new(
        Reduce::new(|_: &mut State<S>, _: &A, _| Effect::none()),
        OptionalStatePath::new(
            |s: &State<S>| s.optional.as_ref(),
            |s: &mut State<S>| s.optional.as_mut(),
        ),
        ActionPath::new(|a: &A| Some(a), |a| a),
        Reduce::new(|_: &mut Child<S>, _: &A, _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut optional, state, action, context);
    send_sync(&optional);
    let mut optional_id = if_some_identified(
        Reduce::new(|_: &mut State<S>, _: &A, _| Effect::none()),
        OptionalStatePath::new(
            |s: &State<S>| s.optional.as_ref(),
            |s: &mut State<S>| s.optional.as_mut(),
        ),
        ActionPath::new(|a: &A| Some(a), |a| a),
        |s: &Child<S>| &s.id,
        Reduce::new(|_: &mut Child<S>, _: &A, _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut optional_id, state, action, context);
    send_sync(&optional_id);
    let mut case = if_case(
        Reduce::new(|_: &mut State<S>, _: &A, _| Effect::none()),
        CaseStatePath::new(
            |s: &State<S>| match &s.case {
                Case::Present(s) => Some(s),
                Case::Absent => None,
            },
            |s: &mut State<S>| match &mut s.case {
                Case::Present(s) => Some(s),
                Case::Absent => None,
            },
        ),
        ActionPath::new(|a: &A| Some(a), |a| a),
        Reduce::new(|_: &mut Child<S>, _: &A, _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut case, state, action, context);
    send_sync(&case);
    let mut case_id = Reduce::new(|_: &mut State<S>, _: &A, _| Effect::none()).if_case_identified(
        CaseStatePath::new(
            |s: &State<S>| match &s.case {
                Case::Present(s) => Some(s),
                Case::Absent => None,
            },
            |s: &mut State<S>| match &mut s.case {
                Case::Present(s) => Some(s),
                Case::Absent => None,
            },
        ),
        ActionPath::new(|a: &A| Some(a), |a| a),
        |s: &Child<S>| &s.id,
        Reduce::new(|_: &mut Child<S>, _: &A, _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut case_id, state, action, context);
    send_sync(&case_id);
}

// C has no Sync bound; its borrowed projection/extraction/identity captures
// cannot be required to be Send or 'static. The actual callbacks remain HRTBs.
fn callbacks<C>(capture: &C, state: &mut State<()>, context: &mut ReduceContext<'_>) {
    let parent = || {
        Reduce::new(|_: &mut State<()>, _: &(), _| {
            std::hint::black_box(capture);
            Effect::none()
        })
    };
    let child = || {
        Reduce::new(|s: &mut Child<()>, _: &(), _| {
            std::hint::black_box((&s.value, capture));
            Effect::none()
        })
    };
    let mut required = scope_identified(
        parent(),
        StatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                &s.required
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                &mut s.required
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        |s: &Child<()>| {
            std::hint::black_box(capture);
            &s.id
        },
        child(),
    );
    let _ = Reducer::reduce(&mut required, state, &(), context);
    let mut required_ext = parent().scope_identified(
        StatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                &s.required
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                &mut s.required
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        captured_identity(capture),
        child(),
    );
    let _ = Reducer::reduce(&mut required_ext, state, &(), context);
    let mut optional = parent().if_some(
        OptionalStatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                s.optional.as_ref()
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                s.optional.as_mut()
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        child(),
    );
    let _ = Reducer::reduce(&mut optional, state, &(), context);
    let mut optional_id = IfSomeIdentified::new(
        parent(),
        OptionalStatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                s.optional.as_ref()
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                s.optional.as_mut()
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        captured_identity(capture),
        child(),
    );
    let _ = Reducer::reduce(&mut optional_id, state, &(), context);
    let mut case = IfCase::new(
        parent(),
        CaseStatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                match &s.case {
                    Case::Present(s) => Some(s),
                    Case::Absent => None,
                }
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                match &mut s.case {
                    Case::Present(s) => Some(s),
                    Case::Absent => None,
                }
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        child(),
    );
    let _ = Reducer::reduce(&mut case, state, &(), context);
    let mut case_id = IfCaseIdentified::new(
        parent(),
        CaseStatePath::new(
            |s: &State<()>| {
                std::hint::black_box(capture);
                match &s.case {
                    Case::Present(s) => Some(s),
                    Case::Absent => None,
                }
            },
            |s: &mut State<()>| {
                std::hint::black_box(capture);
                match &mut s.case {
                    Case::Present(s) => Some(s),
                    Case::Absent => None,
                }
            },
        ),
        ActionPath::new(
            |a: &()| {
                std::hint::black_box(capture);
                Some(a)
            },
            |a| a,
        ),
        |s: &Child<()>| {
            std::hint::black_box(capture);
            &s.id
        },
        child(),
    );
    let _ = Reducer::reduce(&mut case_id, state, &(), context);
}

// Nonempty effects still use an owned lawful mapper, never the local extractor.
fn retained_effect(
    capture: &Rc<Cell<usize>>,
    state: &mut State<&Cell<usize>>,
    context: &mut ReduceContext<'_>,
) {
    let mapper = Arc::new(AtomicUsize::new(0));
    let mut reducer = IfSome::new(
        Reduce::new(|_: &mut State<&Cell<usize>>, _: &usize, _| Effect::none()),
        OptionalStatePath::new(
            |s: &State<&Cell<usize>>| {
                capture.set(capture.get() + 1);
                s.optional.as_ref()
            },
            |s: &mut State<&Cell<usize>>| {
                capture.set(capture.get() + 1);
                s.optional.as_mut()
            },
        ),
        ActionPath::new(
            |a: &usize| {
                capture.set(capture.get() + 1);
                Some(a)
            },
            move |a| {
                mapper.fetch_add(1, Ordering::Relaxed);
                a
            },
        ),
        Reduce::new(|s: &mut Child<&Cell<usize>>, a: &usize, _| {
            s.value.set(*a);
            Effect::send(*a)
        }),
    );
    let _ = Reducer::reduce(&mut reducer, state, &1, context);
}

// These callbacks own their Rc captures and are 'static; only Send is absent.
fn owned_callbacks(context: &mut ReduceContext<'_>) {
    let mut required = scope_identified(
        Reduce::new(|_: &mut State<()>, _: &(), _| Effect::none()),
        StatePath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &State<()>| {
                    capture.set(capture.get() + 1);
                    &s.required
                }
            },
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &mut State<()>| {
                    capture.set(capture.get() + 1);
                    &mut s.required
                }
            },
        ),
        ActionPath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |a: &()| {
                    capture.set(capture.get() + 1);
                    Some(a)
                }
            },
            |a| a,
        ),
        {
            let capture = Rc::new(Cell::new(0));
            move |s: &Child<()>| {
                capture.set(capture.get() + 1);
                &s.id
            }
        },
        Reduce::new(|_: &mut Child<()>, _: &(), _| Effect::none()),
    );
    let mut state = State {
        required: Child { id: 1, value: () },
        optional: Some(Child { id: 2, value: () }),
        case: Case::Present(Child { id: 3, value: () }),
    };
    let _ = Reducer::reduce(&mut required, &mut state, &(), context);
    let mut optional = rca_core::if_some(
        Reduce::new(|_: &mut State<()>, _: &(), _| Effect::none()),
        OptionalStatePath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &State<()>| {
                    capture.set(capture.get() + 1);
                    s.optional.as_ref()
                }
            },
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &mut State<()>| {
                    capture.set(capture.get() + 1);
                    s.optional.as_mut()
                }
            },
        ),
        ActionPath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |a: &()| {
                    capture.set(capture.get() + 1);
                    Some(a)
                }
            },
            |a| a,
        ),
        Reduce::new(|_: &mut Child<()>, _: &(), _| Effect::none()),
    );
    let mut state = State {
        required: Child { id: 1, value: () },
        optional: Some(Child { id: 2, value: () }),
        case: Case::Present(Child { id: 3, value: () }),
    };
    let _ = Reducer::reduce(&mut optional, &mut state, &(), context);
    let mut optional_id = if_some_identified(
        Reduce::new(|_: &mut State<()>, _: &(), _| Effect::none()),
        OptionalStatePath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &State<()>| {
                    capture.set(capture.get() + 1);
                    s.optional.as_ref()
                }
            },
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &mut State<()>| {
                    capture.set(capture.get() + 1);
                    s.optional.as_mut()
                }
            },
        ),
        ActionPath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |a: &()| {
                    capture.set(capture.get() + 1);
                    Some(a)
                }
            },
            |a| a,
        ),
        {
            let capture = Rc::new(Cell::new(0));
            move |s: &Child<()>| {
                capture.set(capture.get() + 1);
                &s.id
            }
        },
        Reduce::new(|_: &mut Child<()>, _: &(), _| Effect::none()),
    );
    let mut state = State {
        required: Child { id: 1, value: () },
        optional: Some(Child { id: 2, value: () }),
        case: Case::Present(Child { id: 3, value: () }),
    };
    let _ = Reducer::reduce(&mut optional_id, &mut state, &(), context);
    let mut case = if_case(
        Reduce::new(|_: &mut State<()>, _: &(), _| Effect::none()),
        CaseStatePath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &State<()>| {
                    capture.set(capture.get() + 1);
                    match &s.case {
                        Case::Present(s) => Some(s),
                        Case::Absent => None,
                    }
                }
            },
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &mut State<()>| {
                    capture.set(capture.get() + 1);
                    match &mut s.case {
                        Case::Present(s) => Some(s),
                        Case::Absent => None,
                    }
                }
            },
        ),
        ActionPath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |a: &()| {
                    capture.set(capture.get() + 1);
                    Some(a)
                }
            },
            |a| a,
        ),
        Reduce::new(|_: &mut Child<()>, _: &(), _| Effect::none()),
    );
    let mut state = State {
        required: Child { id: 1, value: () },
        optional: Some(Child { id: 2, value: () }),
        case: Case::Present(Child { id: 3, value: () }),
    };
    let _ = Reducer::reduce(&mut case, &mut state, &(), context);
    let mut case_id = rca_core::if_case_identified(
        Reduce::new(|_: &mut State<()>, _: &(), _| Effect::none()),
        CaseStatePath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &State<()>| {
                    capture.set(capture.get() + 1);
                    match &s.case {
                        Case::Present(s) => Some(s),
                        Case::Absent => None,
                    }
                }
            },
            {
                let capture = Rc::new(Cell::new(0));
                move |s: &mut State<()>| {
                    capture.set(capture.get() + 1);
                    match &mut s.case {
                        Case::Present(s) => Some(s),
                        Case::Absent => None,
                    }
                }
            },
        ),
        ActionPath::new(
            {
                let capture = Rc::new(Cell::new(0));
                move |a: &()| {
                    capture.set(capture.get() + 1);
                    Some(a)
                }
            },
            |a| a,
        ),
        {
            let capture = Rc::new(Cell::new(0));
            move |s: &Child<()>| {
                capture.set(capture.get() + 1);
                &s.id
            }
        },
        Reduce::new(|_: &mut Child<()>, _: &(), _| Effect::none()),
    );
    let mut state = State {
        required: Child { id: 1, value: () },
        optional: Some(Child { id: 2, value: () }),
        case: Case::Present(Child { id: 3, value: () }),
    };
    let _ = Reducer::reduce(&mut case_id, &mut state, &(), context);
}

fn concrete_local(context: &mut ReduceContext<'_>) {
    // This borrowed payload is Send: only its non-static lifetime is relevant.
    let value = 0;
    let mut borrowed_send = State {
        required: Child {
            id: 1,
            value: &value,
        },
        optional: Some(Child {
            id: 2,
            value: &value,
        }),
        case: Case::Present(Child {
            id: 3,
            value: &value,
        }),
    };
    domains(&mut borrowed_send, &(), context);
    let cell = Cell::new(0);
    let mut state = State {
        required: Child {
            id: 1,
            value: &cell,
        },
        optional: Some(Child {
            id: 2,
            value: &cell,
        }),
        case: Case::Present(Child {
            id: 3,
            value: &cell,
        }),
    };
    domains(&mut state, &Rc::new(()), context);
    retained_effect(&Rc::new(Cell::new(0)), &mut state, context);
    let mut owned = State {
        required: Child {
            id: 1,
            value: Rc::new(()),
        },
        optional: Some(Child {
            id: 2,
            value: Rc::new(()),
        }),
        case: Case::Present(Child {
            id: 3,
            value: Rc::new(()),
        }),
    };
    domains(&mut owned, &(), context);
}

fn main() {
    let _ = callbacks::<AtomicUsize>; // Borrowed-but-Send captures.
    let _ = callbacks::<Rc<Cell<usize>>>; // Borrowed and non-Send captures.
    let _ = concrete_local;
    let _ = owned_callbacks;
    let _ = Case::<()>::Absent;
}
