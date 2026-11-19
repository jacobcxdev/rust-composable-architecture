#![deny(private_interfaces, private_bounds)]

use renamed_action_paths::ExportedPaths;

struct NotDebug;

fn main() {
    fn assert_traits<T: Send + Sync>() {}
    assert_traits::<ExportedPaths<std::rc::Rc<NotDebug>>>();
    let action = ExportedPaths::value().embed(NotDebug);
    assert!(ExportedPaths::value().extract(&action).is_some());
}
