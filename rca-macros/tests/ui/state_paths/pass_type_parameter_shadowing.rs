#![deny(warnings)]

use rca_macros::{ActionPaths, CaseDescriptors, DependencyClient, Snapshot, StatePaths};

macro_rules! identity_type {
    ($type:ty) => {
        $type
    };
}

#[derive(StatePaths)]
struct State<State> {
    value: identity_type!(State),
}

#[derive(ActionPaths, CaseDescriptors)]
enum Event<Event> {
    Value(Event),
}

#[derive(StatePaths)]
struct Record<RecordPaths> {
    value: RecordPaths,
}

#[derive(ActionPaths, CaseDescriptors)]
enum Message<MessagePaths, MessageCase, MessageCaseRef, MessageCases> {
    Value(MessagePaths),
    Payload(MessageCase, MessageCaseRef, MessageCases),
}

trait Project {
    type Value;
}

impl Project for u8 {
    type Value = u16;
}

const WIDTH: usize = 2;

#[derive(StatePaths)]
struct r#Projected<Projected: Project, const N: usize = WIDTH>
where
    Projected::Value: ::core::marker::Copy,
{
    value: <Projected as Project>::Value,
    array: [u8; N],
}

#[derive(DependencyClient)]
struct Client<Client> {
    #[dependency(member = ::core::marker::PhantomData)]
    marker: ::core::marker::PhantomData<Client>,
    #[dependency(test = ::std::sync::Arc::new(|value| value))]
    echo: ::std::sync::Arc<dyn Fn(Client) -> Client + Send + Sync>,
}

#[derive(Snapshot)]
struct History<HistorySnapshot> {
    value: HistorySnapshot,
}

#[derive(Snapshot)]
struct Captured<Captured> {
    value: identity_type!(Captured),
}

#[derive(StatePaths)]
struct Borrowed<'a, Borrowed: ?Sized, const N: usize = WIDTH> {
    bytes: [u8; N],
    #[rca(skip)]
    marker: ::core::marker::PhantomData<&'a Borrowed>,
}

#[derive(Snapshot)]
struct Tuple<TupleSnapshot>(
    u8,
    #[snapshot(omit = "only the value is captured")] ::core::marker::PhantomData<TupleSnapshot>,
    TupleSnapshot,
);

#[derive(Snapshot)]
struct Empty<EmptySnapshot>(
    #[snapshot(omit = "the marker has no historical value")]
    ::core::marker::PhantomData<EmptySnapshot>,
);

#[derive(Snapshot)]
enum Variants<VariantsSnapshot> {
    Empty,
    Value(VariantsSnapshot),
}

#[rca_macros::reducer(
    state(supplied(())),
    action(supplied(())),
    body(supplied(body)),
)]
struct Feature<Feature> {
    body: Feature,
}

fn main() {
    let mut state = State { value: 3_u8 };
    *StatePaths::value().get_mut(&mut state) = 5;
    assert_eq!(*StatePaths::value().get(&state), 5);

    let path = EventPaths::value();
    let event = path.embed(7_u8);
    assert_eq!(path.extract(&event), Some(&7));
    assert_eq!(EventCases::case(&event), EventCase::Value);
    let EventCaseRef::Value(value) = EventCases::project(&event);
    assert_eq!(*value, 7);

    assert_eq!(RecordPaths::value().get(&Record { value: 11 }), &11);
    let message = MessagePaths::<u8, u16, u32, u64>::value().embed(13);
    assert_eq!(MessageCases::case(&message), MessageCase::Value);
    let payload = MessagePaths::<u8, u16, u32, u64>::payload().embed((17, 19, 23));
    let MessageCaseRef::Payload(first, second, third) = MessageCases::project(&payload) else {
        panic!("the generated view changed the selected case");
    };
    assert_eq!((*first, *second, *third), (17, 19, 23));

    let projected = Projected::<u8> {
        value: 29,
        array: [31; WIDTH],
    };
    assert_eq!(ProjectedPaths::value().get(&projected), &29);
    assert_eq!(ProjectedPaths::array().get(&projected), &[31; WIDTH]);
    assert_eq!((Client::<u8>::test_value().echo)(37), 37);

    use rca::Snapshot as _;
    assert_eq!(History { value: 41_u8 }.snapshot().value, 41);
    assert_eq!(Captured { value: 43_u8 }.snapshot().value, 43);
    let borrowed = Borrowed::<str> {
        bytes: [2; WIDTH],
        marker: ::core::marker::PhantomData,
    };
    assert_eq!(BorrowedPaths::bytes().get(&borrowed), &[2; WIDTH]);
    let tuple = Tuple(3, ::core::marker::PhantomData, 5_u8).snapshot();
    assert_eq!((tuple.0, tuple.1), (3, 5));
    let _empty = Empty::<u8>(::core::marker::PhantomData).snapshot();
    assert!(matches!(
        Variants::<u8>::Empty.snapshot(),
        VariantsSnapshot::Empty
    ));
    assert!(matches!(
        Variants::Value(7_u8).snapshot(),
        VariantsSnapshot::Value(7)
    ));

    let feature = Feature {
        body: rca::Reduce::new(|_: &mut (), _: &(), _: &mut rca::ReduceContext<'_>| {
            rca::Effect::none()
        }),
    };
    let _: &dyn rca::Reducer<State = (), Action = ()> = &feature;

    #[derive(StatePaths)]
    struct Local<Local> {
        value: identity_type!(Local),
    }
    assert_eq!(LocalPaths::value().get(&Local { value: 47 }), &47);

    #[expect(
        non_camel_case_types,
        reason = "the source intentionally shares a crate name"
    )]
    #[derive(Snapshot)]
    #[snapshot(name = StdHistory)]
    struct std<std> {
        value: identity_type!(std),
        text: ::std::string::String,
    }
    let captured = std {
        value: 53_u8,
        text: ::std::string::String::from("kept"),
    }
    .snapshot();
    assert_eq!((captured.value, captured.text.as_str()), (53, "kept"));
}
