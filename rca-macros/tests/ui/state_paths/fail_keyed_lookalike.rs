use rca_macros::StatePaths;

struct KeyedState<Id, Child>(rca::KeyedState<Id, Child>);

impl<Id, Child> std::ops::Deref for KeyedState<Id, Child> {
    type Target = rca::KeyedState<Id, Child>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<Id, Child> std::ops::DerefMut for KeyedState<Id, Child> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(StatePaths)]
struct State {
    #[rca(keyed)]
    child: KeyedState<u8, u8>,
}

fn main() {}
