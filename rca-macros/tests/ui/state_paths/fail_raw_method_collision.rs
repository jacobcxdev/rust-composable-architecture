// Treats raw and ordinary case spellings alike when detecting collisions.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Foo(u8),
    #[expect(
        non_camel_case_types,
        reason = "the fixture verifies raw and non-raw method collision detection"
    )]
    r#foo(u8),
}

fn main() {}
