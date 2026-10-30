/// Selects cases while retaining the supported subset of ordinary test-harness controls.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Mirrors named harness CLI switches; incompatible flags are rejected during parsing."
)]
struct Options {
    /// Selects the union of these names or substrings; an empty list selects all.
    filters: Vec<String>,
    /// Excludes matching cases after positive selection.
    skips: Vec<String>,
    /// Applies equality rather than substring matching to filters and exclusions.
    exact: bool,
    /// Prints selected names without invoking measurement functions.
    list: bool,
    /// Selects nothing because this roster has no ignored cases.
    ignored: bool,
    /// Accepts the ordinary harness flag without changing this non-ignored roster.
    include_ignored: bool,
    /// Stops dispatch after the first caught test panic.
    fail_fast: bool,
}

impl Options {
    /// Parses supported controls without silently treating unknown flags as filters.
    ///
    /// # Errors
    /// Rejects unknown flags, missing values, invalid thread counts and conflicting ignored controls.
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--exact" => options.exact = true,
                "--list" => options.list = true,
                "--nocapture" => {}
                "--ignored" => options.ignored = true,
                "--include-ignored" => options.include_ignored = true,
                "--fail-fast" => options.fail_fast = true,
                "--skip" => {
                    let filter = args.next().ok_or("--skip requires a filter")?;
                    if filter.starts_with('-') {
                        return Err(
                            "--skip requires a filter; use --skip= for hyphen-prefixed filters"
                                .to_owned(),
                        );
                    }
                    options.skips.push(filter.clone());
                }
                "--test-threads" => Self::threads(
                    args.next()
                        .ok_or("--test-threads requires a positive integer")?,
                )?,
                "--" => {
                    options.filters.extend(args.cloned());
                    break;
                }
                value if value.starts_with("--skip=") => {
                    options.skips.push(value["--skip=".len()..].to_owned());
                }
                value if value.starts_with("--test-threads=") => {
                    Self::threads(&value["--test-threads=".len()..])?;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unsupported option: {value}"));
                }
                _ => options.filters.push(argument.clone()),
            }
        }
        if options.ignored && options.include_ignored {
            return Err("--ignored and --include-ignored cannot be combined".to_owned());
        }
        Ok(options)
    }

    /// Accepts positive harness thread counts while allocation cases remain sequential.
    ///
    /// # Errors
    /// Rejects zero, negative, non-numeric and out-of-range values.
    fn threads(value: &str) -> Result<(), String> {
        match value.parse::<usize>() {
            Ok(threads) if threads > 0 => Ok(()),
            _ => Err("--test-threads requires a positive integer".to_owned()),
        }
    }

    /// Applies positive selection before exclusions, without invoking a case.
    fn matches(&self, name: &str) -> bool {
        let matches = |filter: &String| {
            if self.exact {
                name == filter
            } else {
                name.contains(filter)
            }
        };
        !self.ignored
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(matches)
    }
}

/// Rejects child markers that could skip or repeat the intended measurement.
///
/// # Errors
/// Rejects listing, ambiguous selection, conflicting markers and mismatched case families.
fn validate_child(
    selected: &[&(&str, fn())],
    list: bool,
    keyed_child: bool,
    shared_child: Option<&std::ffi::OsStr>,
    scope_child: bool,
) -> Result<(), String> {
    let markers =
        usize::from(keyed_child) + usize::from(shared_child.is_some()) + usize::from(scope_child);
    if markers == 0 {
        return Ok(());
    }
    if list || selected.len() != 1 || markers != 1 {
        return Err("allocation child requires exactly one matching executable case".to_owned());
    }
    let name = selected[0].0;
    let is_keyed = name.starts_with("composition::keyed::allocation_tests::");
    let is_scope =
        name == "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership";
    if keyed_child != is_keyed
        || scope_child != is_scope
        || shared_child.is_some_and(|marker| marker != std::ffi::OsStr::new(name))
    {
        return Err("allocation child marker does not match the selected case".to_owned());
    }
    Ok(())
}

/// Lists or sequentially runs selected cases and reports whether every case passed.
///
/// Each case owns its profiler. A case panic becomes a failed test, not an
/// application recovery policy. Abort-mode panics cannot be caught here.
///
/// # Errors
/// Returns invalid option or child-marker errors before invoking measurements.
pub(crate) fn run(
    args: &[String],
    keyed_child: bool,
    shared_child: Option<&std::ffi::OsStr>,
    scope_child: bool,
) -> Result<bool, String> {
    let options = Options::parse(args)?;
    let selected: Vec<_> = CASES
        .iter()
        .filter(|case| options.matches(case.0))
        .collect();
    validate_child(
        &selected,
        options.list,
        keyed_child,
        shared_child,
        scope_child,
    )?;
    if options.list {
        for case in &selected {
            println!("{}: test", case.0);
        }
        println!("\n{} tests", selected.len());
        return Ok(true);
    }

    let mut passed = 0;
    let mut failed = 0;
    for case in selected {
        // The unchanged case owns profiler creation and teardown inside this call.
        let result = std::panic::catch_unwind(case.1);
        if result.is_ok() {
            passed += 1;
            println!("test {} ... ok", case.0);
        } else {
            failed += 1;
            println!("test {} ... FAILED", case.0);
            if options.fail_fast {
                break;
            }
        }
    }
    println!(
        "\ntest result: {}. {passed} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" }
    );
    Ok(failed == 0)
}
