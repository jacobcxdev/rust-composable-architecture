// Accepts concrete and generic aliases of RCA's keyed action.

use rca::KeyedAction;
use rca_macros::ActionPaths;

type RowPayload = KeyedAction<u8, u16>;
type GenericRowPayload<Id, Action> = KeyedAction<Id, Action>;

#[derive(ActionPaths)]
enum Action {
    #[rca(keyed)]
    Row(RowPayload),
}

#[derive(ActionPaths)]
enum GenericAction<Id> {
    #[rca(keyed)]
    Row(GenericRowPayload<Id, u16>),
}

fn main() {
    let path = ActionPaths::row();
    let action = path.embed(7, 11);
    assert!(matches!(path.extract(&action), Some((7, 11))));

    let generic_path = GenericActionPaths::<u8>::row();
    let generic_action = generic_path.embed(13, 17);
    assert!(matches!(
        generic_path.extract(&generic_action),
        Some((13, 17))
    ));
}
