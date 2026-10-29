#![deny(private_interfaces, private_bounds)]

use renamed_state_paths::{Exported, ExportedPaths};

struct NotDebug;

fn main() {
    fn assert_traits<T: Send + Sync>() {}
    assert_traits::<ExportedPaths<std::rc::Rc<NotDebug>>>();
    let state = Exported { value: NotDebug };
    let _ = ExportedPaths::value().get(&state);
}
