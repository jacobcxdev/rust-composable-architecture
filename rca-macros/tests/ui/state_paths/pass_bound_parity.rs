// Checks that generated state paths need no bounds beyond manual paths.

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use rca::{KeyedState, KeyedStatePath, OptionalStatePath, StatePath};
use rca_macros::StatePaths;

struct NonCloneNonDebug;

struct Id(u8);

impl PartialEq for Id {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Id {}

impl Hash for Id {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[derive(StatePaths)]
struct AppState<Id, Row>
where
    Id: Eq + Hash,
{
    child: NonCloneNonDebug,
    #[rca(optional)]
    detail: Option<NonCloneNonDebug>,
    #[rca(keyed)]
    rows: KeyedState<Id, Row>,
}

fn paths_compile_without_clone_or_debug_bounds() {
    let generated_required = AppStatePaths::<Id, NonCloneNonDebug>::child();
    let generated_optional = AppStatePaths::<Id, NonCloneNonDebug>::detail();
    let generated_keyed = AppStatePaths::<Id, NonCloneNonDebug>::rows();
    let manual_required = StatePath::new(
        |state: &AppState<Id, NonCloneNonDebug>| &state.child,
        |state: &mut AppState<Id, NonCloneNonDebug>| &mut state.child,
    );
    let manual_optional = OptionalStatePath::new(
        |state: &AppState<Id, NonCloneNonDebug>| state.detail.as_ref(),
        |state: &mut AppState<Id, NonCloneNonDebug>| state.detail.as_mut(),
    );
    let manual_keyed = KeyedStatePath::new(
        |state: &AppState<Id, NonCloneNonDebug>| &state.rows,
        |state: &mut AppState<Id, NonCloneNonDebug>| &mut state.rows,
    );
    let mut state = AppState {
        child: NonCloneNonDebug,
        detail: Some(NonCloneNonDebug),
        rows: KeyedState::new(),
    };

    let _ = generated_required.get(&state);
    let _ = generated_optional.get(&state);
    let _ = generated_keyed.get(&state);
    let _ = manual_required.get_mut(&mut state);
    let _ = manual_optional.get_mut(&mut state);
    let _ = manual_keyed.get_mut(&mut state);
}

#[derive(StatePaths)]
struct NonSendState {
    child: Rc<Cell<u8>>,
}

#[derive(StatePaths)]
struct BorrowedState<'a> {
    child: &'a Cell<u8>,
}

fn paths_compile_without_send_sync_or_static_bounds<'a>(child: &'a Cell<u8>) {
    let generated_non_send = NonSendStatePaths::child();
    let manual_non_send = StatePath::new(
        |state: &NonSendState| &state.child,
        |state: &mut NonSendState| &mut state.child,
    );
    let generated_borrowed = BorrowedStatePaths::child();
    let manual_borrowed = StatePath::new(
        |state: &BorrowedState<'a>| &state.child,
        |state: &mut BorrowedState<'a>| &mut state.child,
    );
    let mut non_send = NonSendState {
        child: Rc::new(Cell::new(0)),
    };
    let mut borrowed = BorrowedState { child };

    let _ = generated_non_send.get_mut(&mut non_send);
    let _ = manual_non_send.get_mut(&mut non_send);
    let _ = generated_borrowed.get_mut(&mut borrowed);
    let _ = manual_borrowed.get_mut(&mut borrowed);
}

fn main() {
    paths_compile_without_clone_or_debug_bounds();
    let child = Cell::new(0);
    paths_compile_without_send_sync_or_static_bounds(&child);
}
