use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[r#dependency(r#unknown = 1)]
struct UnsupportedShape {
    value: u8,
}

#[derive(DependencyKey)]
#[dependency(unknown = 1)]
#[r#dependency(r#value = u8, r#live = 1)]
struct DuplicateAttributes;

#[derive(DependencyKey)]
#[dependency(value = u8, r#value = u16, live = 1)]
struct DuplicateValue;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1, r#live = 2)]
struct DuplicateLive;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1, test = 2, r#test = 3)]
struct DuplicateTest;

#[derive(DependencyKey)]
#[dependency(value = u8, r#nested::r#live = 1)]
struct QualifiedOption;

fn main() {}
