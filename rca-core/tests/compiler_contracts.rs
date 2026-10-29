//! Compiler diagnostics for contracts that include successful warnings.

#[cfg(not(miri))]
use ui_test::dependencies::DependencyBuilder;

#[cfg(not(miri))]
fn main() -> ui_test::color_eyre::Result<()> {
    // Cargo runs integration tests from the package root, not the caller's directory.
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        ui_test::color_eyre::eyre::ensure!(
            std::path::Path::new(&target).is_absolute(),
            "compiler contracts require an absolute CARGO_TARGET_DIR; unset it to use Cargo's default"
        );
    }
    let mut config = ui_test::Config::rustc("tests/compiler/pass");
    config.out_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"),
            std::path::PathBuf::from,
        )
        .join("compiler-contracts/core");
    config.program.args.push("--emit=metadata".into());
    config.comment_defaults.base().add_custom(
        "edition",
        ui_test::custom_flags::edition::Edition("2024".into()),
    );
    config.comment_defaults.base().add_custom("no-rustfix", ());
    config.comment_defaults.base().require_annotations_for_level =
        ui_test::spanned::Spanned::dummy(ui_test::diagnostics::Level::Warn).into();
    let mut dependencies = DependencyBuilder::default();
    // Reuse Cargo's workspace target instead of a separate dependency build per suite.
    dependencies.program.out_dir_flag = None;
    dependencies.program.args.extend(
        [
            "--locked",
            "--lib",
            "--profile",
            "test",
            "--no-default-features",
        ]
        .map(Into::into),
    );
    let features = [
        (cfg!(feature = "local"), "local"),
        (cfg!(feature = "diagnostics"), "diagnostics"),
        (cfg!(feature = "test-util"), "test-util"),
        (cfg!(feature = "futures-stream"), "futures-stream"),
        (
            cfg!(feature = "allocation-measurement"),
            "allocation-measurement",
        ),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect::<Vec<_>>();
    if !features.is_empty() {
        dependencies.program.args.push("--features".into());
        dependencies.program.args.push(features.join(",").into());
    }
    config
        .comment_defaults
        .base()
        .add_custom("dependencies", dependencies);
    let mut rejected = config.clone();
    rejected.root_dir = "tests/compiler/fail".into();
    config.comment_defaults.base().exit_status = ui_test::spanned::Spanned::dummy(0).into();
    let args = ui_test::Args::test()?;
    config.with_args(&args);
    rejected.with_args(&args);
    let emitter: Box<dyn ui_test::status_emitter::StatusEmitter> = args.format.into();
    ui_test::run_tests_generic(
        vec![config, rejected],
        ui_test::default_file_filter,
        ui_test::default_per_file_config,
        emitter,
    )
}

/// Leaves compiler subprocess checks to the native test jobs.
#[cfg(miri)]
fn main() {
    eprintln!("compiler contracts skipped: Miri cannot launch rustc subprocesses");
}
