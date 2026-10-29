// Checks path kinds, container aliases and field visibility independently.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

use rca::{KeyedState, StatePath};
use rca_macros::StatePaths;

struct NonCloneNonDebug;
type OptionalAlias = Option<NonCloneNonDebug>;
type KeyedAlias<Id, Row> = KeyedState<Id, Row>;

mod nested {
    use super::*;

    #[derive(StatePaths)]
    pub struct PublicState<Id, Row>
    where
        Id: Eq + Hash,
    {
        pub child: NonCloneNonDebug,
        #[rca(required)]
        pub required_child: NonCloneNonDebug,
        #[rca(optional)]
        pub detail: Option<NonCloneNonDebug>,
        #[rca(keyed)]
        pub rows: KeyedState<Id, Row>,
        pub r#type: Option<NonCloneNonDebug>,
        pub optional_alias: OptionalAlias,
        pub unannotated_keyed: KeyedState<Id, Row>,
        pub keyed_alias: KeyedAlias<Id, Row>,
        pub hash_map: HashMap<Id, Row>,
        pub btree_map: BTreeMap<Id, Row>,
        #[rca(skip)]
        pub ignored: NonCloneNonDebug,
    }

    #[derive(StatePaths)]
    pub(crate) struct CrateState {
        child: NonCloneNonDebug,
    }

    #[derive(StatePaths)]
    struct PrivateState {
        child: NonCloneNonDebug,
    }

    #[derive(StatePaths)]
    pub struct VisibilityState {
        pub public_child: NonCloneNonDebug,
        pub(crate) crate_child: NonCloneNonDebug,
        private_child: NonCloneNonDebug,
    }

    pub fn paths_compile<Id, Row>()
    where
        Id: Eq + Hash,
    {
        let _ = PublicStatePaths::<Id, Row>::child();
        let _ = PublicStatePaths::<Id, Row>::required_child();
        let _ = PublicStatePaths::<Id, Row>::detail();
        let _ = PublicStatePaths::<Id, Row>::rows();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::r#type();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::optional_alias();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::unannotated_keyed();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::keyed_alias();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::hash_map();
        let _: StatePath<_, _, _, _> = PublicStatePaths::<Id, Row>::btree_map();
        let _ = CrateStatePaths::child();
        let _ = PrivateStatePaths::child();
        let _ = VisibilityStatePaths::private_child();
    }
}

fn main() {
    nested::paths_compile::<u8, NonCloneNonDebug>();
    let _ = nested::VisibilityStatePaths::public_child();
    let _ = nested::VisibilityStatePaths::crate_child();
}
