use rca_macros::StatePaths;

struct Option<T>(T);

impl<T> Option<T> {
    fn as_ref(&self) -> std::option::Option<&T> {
        Some(&self.0)
    }
    fn as_mut(&mut self) -> std::option::Option<&mut T> {
        Some(&mut self.0)
    }
}

#[derive(StatePaths)]
struct State {
    #[rca(optional)]
    child: Option<u8>,
}

fn main() {}
