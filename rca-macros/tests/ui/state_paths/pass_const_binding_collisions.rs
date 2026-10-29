#![expect(
    non_upper_case_globals,
    reason = "lowercase constants deliberately collide with generated bindings"
)]
#![deny(unused_imports)]
const state: usize = 17;
const state_: usize = 17;
const state___: usize = 17;
const state____: usize = 19;
macro_rules! array_type {
    () => {
        [u8; state____]
    };
}
const child: usize = 17;
const child___: usize = 17;
use rca_macros::StatePaths;

#[derive(StatePaths)]
struct Record<const state: usize, const state_: usize, const state__: usize> {
    state: u8,
    array: [u8; state___],
    macro_array: array_type!(),
    #[rca(optional)]
    optional: Option<u8>,
    #[rca(keyed)]
    keyed: rca::KeyedState<u8, u8>,
}

#[derive(StatePaths)]
enum Cases<const state: usize, const child: usize, const child_: usize, const child__: usize> {
    Tuple(u8),
    Named { child: u8 },
}

fn main() {
    let mut value = Record::<1, 2, 3> {
        array: [0; state___],
        macro_array: [0; state____],
        state: 4,
        optional: Some(5),
        keyed: [(1, 6)].into_iter().collect(),
    };
    *RecordPaths::<1, 2, 3>::state().get_mut(&mut value) = 7;
    assert_eq!(*RecordPaths::<1, 2, 3>::state().get(&value), 7);
    assert_eq!(RecordPaths::<1, 2, 3>::optional().get(&value), Some(&5));
    assert_eq!(
        RecordPaths::<1, 2, 3>::keyed().get(&value).get(&1),
        Some(&6)
    );
    let mut tuple = Cases::<1, 2, 3, 4>::Tuple(8);
    *CasesPaths::<1, 2, 3, 4>::tuple()
        .get_mut(&mut tuple)
        .unwrap() = 9;
    assert_eq!(CasesPaths::<1, 2, 3, 4>::tuple().get(&tuple), Some(&9));
    let mut named = Cases::<1, 2, 3, 4>::Named { child: 10 };
    assert_eq!(CasesPaths::<1, 2, 3, 4>::child().get(&named), Some(&10));
    *CasesPaths::<1, 2, 3, 4>::child()
        .get_mut(&mut named)
        .unwrap() = 11;
    assert_eq!(CasesPaths::<1, 2, 3, 4>::tuple().get(&named), None);
    assert_eq!(CasesPaths::<1, 2, 3, 4>::tuple().get_mut(&mut named), None);
    assert_eq!(CasesPaths::<1, 2, 3, 4>::child().get(&tuple), None);
    assert_eq!(CasesPaths::<1, 2, 3, 4>::child().get_mut(&mut tuple), None);
    assert_eq!(CasesPaths::<1, 2, 3, 4>::tuple().get(&tuple), Some(&9));
    assert_eq!(CasesPaths::<1, 2, 3, 4>::child().get(&named), Some(&11));
}
