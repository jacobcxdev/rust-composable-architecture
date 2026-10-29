#![expect(
    non_upper_case_globals,
    reason = "lowercase constants deliberately collide with generated bindings"
)]
#![deny(unused_imports)]
const action: usize = 17;
const action__: usize = 17;
const payload: usize = 17;
const payload___: usize = 17;
macro_rules! array_type {
    () => {
        [u8; action__]
    };
}
use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action<
    const r#action: usize,
    const action_: usize,
    const payload: usize,
    const payload_: usize,
    const payload__: usize,
> {
    Tuple(u8),
    Array(array_type!()),
    Named {
        payload: u8,
    },
    #[rca(keyed)]
    Keyed(rca::KeyedAction<u8, u8>),
    #[rca(keyed)]
    NamedKeyed {
        payload: rca::KeyedAction<u8, u8>,
    },
}

fn main() {
    let array = ActionPaths::<1, 2, 3, 4, 5>::array();
    assert_eq!(
        array.extract(&array.embed([7; action__])),
        Some(&[7; action__])
    );
    let tuple = ActionPaths::<1, 2, 3, 4, 5>::tuple();
    assert_eq!(tuple.extract(&tuple.embed(6)), Some(&6));
    let named = ActionPaths::<1, 2, 3, 4, 5>::named();
    assert_eq!(named.extract(&named.embed(7)), Some(&7));
    assert_eq!(named.extract(&tuple.embed(6)), None);
    let keyed = ActionPaths::<1, 2, 3, 4, 5>::keyed();
    assert_eq!(keyed.extract(&keyed.embed(8, 9)), Some((&8, &9)));
    let named_keyed = ActionPaths::<1, 2, 3, 4, 5>::named_keyed();
    assert_eq!(
        named_keyed.extract(&named_keyed.embed(10, 11)),
        Some((&10, &11))
    );
    assert_eq!(named_keyed.extract(&keyed.embed(8, 9)), None);
}
