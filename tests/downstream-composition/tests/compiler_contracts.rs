//! Checks packaged facade ownership and task warnings without executing fixture code.

use ui_test::dependencies::DependencyBuilder;

/// Runs successful and rejected fixtures against only this consumer's declared dependencies.
///
/// # Errors
/// Returns invalid configuration, compiler or diagnostic-comparison failures.
fn main() -> ui_test::color_eyre::Result<()> {
    // Cargo runs integration tests from the package root, not the caller's directory.
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        ui_test::color_eyre::eyre::ensure!(
            std::path::Path::new(&target).is_absolute(),
            "compiler contracts require an absolute CARGO_TARGET_DIR; unset it to use Cargo's default"
        );
    }
    let mut accepted = ui_test::Config::rustc("tests/compiler/pass");
    accepted.out_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
            std::path::PathBuf::from,
        )
        .join("compiler-contracts/downstream-composition");
    accepted.program.args.push("--emit=metadata".into());
    accepted.comment_defaults.base().add_custom(
        "edition",
        ui_test::custom_flags::edition::Edition("2024".into()),
    );
    accepted
        .comment_defaults
        .base()
        .add_custom("no-rustfix", ());
    accepted
        .comment_defaults
        .base()
        .require_annotations_for_level =
        ui_test::spanned::Spanned::dummy(ui_test::diagnostics::Level::Warn).into();

    let mut dependencies = DependencyBuilder::default();
    // Share Cargo's target directory; the sample depends only on packaged APIs.
    dependencies.program.out_dir_flag = None;
    dependencies
        .program
        .args
        .extend(["--locked", "--profile", "test", "--bins"].map(Into::into));
    accepted
        .comment_defaults
        .base()
        .add_custom("dependencies", dependencies);

    let mut rejected = accepted.clone();
    rejected.root_dir = "tests/compiler/fail".into();
    accepted.comment_defaults.base().exit_status = ui_test::spanned::Spanned::dummy(0).into();
    let args = ui_test::Args::test()?;
    accepted.with_args(&args);
    rejected.with_args(&args);
    let emitter: Box<dyn ui_test::status_emitter::StatusEmitter> = args.format.into();
    ui_test::run_tests_generic(
        vec![accepted, rejected],
        ui_test::default_file_filter,
        ui_test::default_per_file_config,
        emitter,
    )
}
