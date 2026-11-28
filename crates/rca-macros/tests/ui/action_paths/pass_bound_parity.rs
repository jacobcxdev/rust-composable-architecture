// Checks that generated action paths need no bounds beyond manual paths.

use std::cell::Cell;
use std::rc::Rc;

use rca::{ActionPath, KeyedAction, KeyedActionPath};
use rca_macros::ActionPaths;

struct NonCloneNonDebug;

#[derive(ActionPaths)]
enum AppAction<'a, Id, Row> {
    Child(Rc<Cell<u8>>),
    Borrowed(&'a NonCloneNonDebug),
    #[rca(keyed)]
    Row(KeyedAction<Id, Row>),
}

fn paths_compile_without_clone_debug_send_sync_or_static_bounds<'a>(
    borrowed: &'a NonCloneNonDebug,
) {
    let generated_child = AppActionPaths::<'a, u8, NonCloneNonDebug>::child();
    let generated_borrowed = AppActionPaths::<'a, u8, NonCloneNonDebug>::borrowed();
    let generated_row = AppActionPaths::<'a, u8, NonCloneNonDebug>::row();
    let manual_child = ActionPath::new(
        |action: &AppAction<'a, u8, NonCloneNonDebug>| match action {
            AppAction::Child(child) => Some(child),
            _ => None,
        },
        AppAction::Child,
    );
    let manual_borrowed = ActionPath::new(
        |action: &AppAction<'a, u8, NonCloneNonDebug>| match action {
            AppAction::Borrowed(child) => Some(child),
            _ => None,
        },
        AppAction::Borrowed,
    );
    let manual_row = KeyedActionPath::new(
        |action: &AppAction<'a, u8, NonCloneNonDebug>| match action {
            AppAction::Row(row) => Some(row),
            _ => None,
        },
        AppAction::Row,
    );
    let _ = generated_child.embed(Rc::new(Cell::new(0)));
    let _ = generated_borrowed.embed(borrowed);
    let _ = generated_row.embed(7, NonCloneNonDebug);
    let _ = manual_child.embed(Rc::new(Cell::new(0)));
    let _ = manual_borrowed.embed(borrowed);
    let _ = manual_row.embed(7, NonCloneNonDebug);
}

fn main() {
    let borrowed = NonCloneNonDebug;
    paths_compile_without_clone_debug_send_sync_or_static_bounds(&borrowed);
}
