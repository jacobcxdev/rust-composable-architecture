//! Checks borrowing action paths, dependency aliases and rejection diagnostics.
//!
//! Nested Cargo commands inherit the build job's output streams and process group.
//! The caller owns timeout and cancellation for Cargo and its descendants.

use std::path::Path;
use std::process::Command;

use rca::{ActionPath, KeyedAction, KeyedActionPath};
use rca_macros::ActionPaths;

struct ChildAction(String);

struct RowAction(u8);

#[derive(ActionPaths)]
enum AppAction<Id> {
    Child(ChildAction),
    #[rca(keyed)]
    Row(KeyedAction<Id, RowAction>),
    SetName {
        name: String,
    },
    Parent,
}

#[test]
fn derives_borrowing_extractors_and_owned_embedders() {
    let child: ActionPath<_, _, _, _> = AppActionPaths::<u8>::child();
    let row: KeyedActionPath<_, _, _, _, _> = AppActionPaths::<u8>::row();
    let name: ActionPath<_, _, _, _> = AppActionPaths::<u8>::set_name();

    let child_action = child.embed(ChildAction(String::from("child")));
    assert!(matches!(child.extract(&child_action), Some(ChildAction(value)) if value == "child"));
    assert!(child.extract(&AppAction::Parent).is_none());

    let row_action = row.embed(7, RowAction(3));
    assert!(matches!(row.extract(&row_action), Some((7, RowAction(3)))));
    assert!(row.extract(&AppAction::Parent).is_none());

    let name_action = name.embed(String::from("Ada"));
    assert!(matches!(name.extract(&name_action), Some(value) if value == "Ada"));
}

#[test]
fn action_paths_qualification() {
    renamed_umbrella_fixture_compiles();
    renamed_core_fixture_compiles();
    crate_self_fixture_compiles();
    keyword_alias_fixture_compiles();
    action_paths_compile_matrix();
}

fn renamed_umbrella_fixture_compiles() {
    fixture_compiles("renamed-action-paths");
}

fn renamed_core_fixture_compiles() {
    fixture_compiles("renamed-core-action-paths");
}

fn crate_self_fixture_compiles() {
    fixture_compiles("crate-self-action-paths");
}

fn keyword_alias_fixture_compiles() {
    fixture_compiles("renamed-keyword-action-paths");
}

fn fixture_compiles(name: &str) {
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}/Cargo.toml"));
    let target = std::path::PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()),
    )
    .join("action-path-fixture");
    let status = Command::new(env!("CARGO"))
        .args(["check", "--manifest-path"])
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .env("CARGO_BUILD_JOBS", "1")
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| {
            panic!("{name} ActionPaths Cargo check could not complete: {error}")
        });

    assert!(
        status.success(),
        "{name} ActionPaths fixture failed: {status}"
    );
}

fn action_paths_compile_matrix() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/action_paths/pass_*.rs");
    tests.compile_fail("tests/ui/action_paths/fail_*.rs");
}
