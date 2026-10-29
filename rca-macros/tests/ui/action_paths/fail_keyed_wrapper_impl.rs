// Prevents a wrapper from implementing RCA's sealed keyed-action contract.

use rca::{KeyedAction, KeyedActionPayload};

struct WrongKeyed<Id, Action>(KeyedAction<Id, Action>);

impl<Id, Action> KeyedActionPayload for WrongKeyed<Id, Action> {
    type Id = Id;
    type Action = Action;

    fn as_keyed_action(&self) -> &KeyedAction<Self::Id, Self::Action> {
        &self.0
    }

    fn from_keyed_action(action: KeyedAction<Self::Id, Self::Action>) -> Self {
        Self(action)
    }
}

fn main() {}
