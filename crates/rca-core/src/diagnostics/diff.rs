//! Produces bounded context around the first rendered snapshot difference.

use std::fmt::Write;

/// Context on each side of a change, without printing the entire snapshot.
const CONTEXT_LINES: usize = 3;
/// Maximum displayed changed lines per side, followed by an omission count.
const CHANGED_LINES: usize = 16;
/// Displayed scalars per line, including numbering and the truncation marker.
const LINE_SCALARS: usize = 240;
/// Suffix that distinguishes a shortened line from the caller's complete text.
const TRUNCATION: &str = " [line truncated]";

/// Renders selected context after the caller establishes semantic inequality.
///
/// Identical renderings retain that inequality rather than hiding a redacted change.
/// Display limits do not bound source scans or the temporary line-index storage.
pub(super) fn excerpt(expected: &str, actual: &str, before: &str, after: &str) -> String {
    let mut output = String::new();
    append_excerpt(&mut output, expected, actual, before, after);
    output
}

/// Appends a state excerpt without rebuilding the caller's existing output prefix.
pub(super) fn append_excerpt(
    output: &mut String,
    expected: &str,
    actual: &str,
    before: &str,
    after: &str,
) {
    if expected == actual {
        output.push_str(
            "Snapshots differ semantically, but their rendered representations are identical.",
        );
        return;
    }
    // Retain trailing and empty lines so newline-only differences remain visible.
    let expected: Vec<_> = expected.split('\n').collect();
    let actual: Vec<_> = actual.split('\n').collect();
    let prefix = expected
        .iter()
        .zip(&actual)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = expected[prefix..]
        .iter()
        .rev()
        .zip(actual[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    side(output, before, '-', &expected, prefix, suffix);
    side(output, after, '+', &actual, prefix, suffix);
    output.push_str("Later differences may be omitted.");
}

/// Renders one side using non-overlapping common prefix and suffix lengths.
///
/// The caller computes both lengths from the same line slices. Trailing empty
/// lines remain present, so insertion and deletion of a newline stay visible.
fn side(
    output: &mut String,
    label: &str,
    marker: char,
    lines: &[&str],
    prefix: usize,
    suffix: usize,
) {
    writeln!(output, "{label}:").expect("writing to String cannot fail");
    let start = prefix.saturating_sub(CONTEXT_LINES);
    if start > 0 {
        writeln!(output, "  ... {start} preceding lines omitted ...")
            .expect("writing to String cannot fail");
    }
    let changed_end = lines.len() - suffix;
    let shown_end = changed_end.min(prefix + CHANGED_LINES);
    for (index, line) in lines.iter().enumerate().take(shown_end).skip(start) {
        display_line(
            output,
            if index < prefix { ' ' } else { marker },
            index + 1,
            line,
        );
    }
    if shown_end < changed_end {
        writeln!(
            output,
            "  ... {} changed lines omitted ...",
            changed_end - shown_end
        )
        .expect("writing to String cannot fail");
    }
    let context_end = lines.len().min(changed_end + CONTEXT_LINES);
    for (index, line) in lines.iter().enumerate().take(context_end).skip(changed_end) {
        display_line(output, ' ', index + 1, line);
    }
    if context_end < lines.len() {
        writeln!(
            output,
            "  ... {} following lines omitted ...",
            lines.len() - context_end
        )
        .expect("writing to String cannot fail");
    }
}

/// Appends one numbered line without allocating separate truncation storage.
///
/// Internal markers and `usize` line numbers leave space for the truncation suffix.
/// Character indices preserve UTF-8 boundaries while the limit counts Unicode scalars.
fn display_line(output: &mut String, marker: char, number: usize, line: &str) {
    let start = output.len();
    write!(output, "{marker} {number} | ").expect("writing to String cannot fail");
    let available = LINE_SCALARS - output[start..].chars().count();
    if line.chars().nth(available).is_some() {
        let (end, _) = line
            .char_indices()
            .nth(available - TRUNCATION.chars().count())
            .expect("a truncated line contains its retained prefix");
        output.push_str(&line[..end]);
        output.push_str(TRUNCATION);
    } else {
        output.push_str(line);
    }
    output.push('\n');
}

/// Renders bounded action text and counts omitted lines without formatting domain values.
pub(super) fn action(rendered: &str) -> String {
    let mut output = String::from("action:\n");
    let mut lines = rendered.split('\n');
    for (index, line) in lines.by_ref().take(CHANGED_LINES).enumerate() {
        display_line(&mut output, ' ', index + 1, line);
    }
    let omitted = lines.count();
    if omitted > 0 {
        writeln!(output, "  ... {omitted} action lines omitted ...")
            .expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::{action, display_line, excerpt};

    #[test]
    fn line_rendering_preserves_exact_unicode_boundaries_and_numbers() {
        let cases = [
            (' ', 1, String::new(), "  1 | \n".to_owned()),
            ('-', 9, "ASCII".to_owned(), "- 9 | ASCII\n".to_owned()),
            ('+', 10, "界🙂é".to_owned(), "+ 10 | 界🙂é\n".to_owned()),
            (
                '-',
                1,
                "a".repeat(234),
                format!("- 1 | {}\n", "a".repeat(234)),
            ),
            (
                '-',
                1,
                "a".repeat(235),
                format!("- 1 | {} [line truncated]\n", "a".repeat(217)),
            ),
            (
                '+',
                10,
                "界".repeat(233),
                format!("+ 10 | {}\n", "界".repeat(233)),
            ),
            (
                '+',
                10,
                "界".repeat(234),
                format!("+ 10 | {} [line truncated]\n", "界".repeat(216)),
            ),
            (
                ' ',
                100,
                "🙂".repeat(233),
                format!("  100 | {} [line truncated]\n", "🙂".repeat(215)),
            ),
        ];
        for (marker, number, input, expected) in cases {
            let mut output = "existing output\n".to_owned();
            display_line(&mut output, marker, number, &input);
            assert_eq!(output, format!("existing output\n{expected}"));
        }
    }

    #[test]
    fn action_and_mismatch_keep_empty_and_trailing_lines() {
        assert_eq!(action(""), "action:\n  1 | \n");
        assert_eq!(action("a\nβ\n"), "action:\n  1 | a\n  2 | β\n  3 | \n");
        assert_eq!(
            excerpt("a\n", "a\nβ\n", "before", "after"),
            "before:\n  1 | a\n  2 | \nafter:\n  1 | a\n+ 2 | β\n  3 | \nLater differences may be omitted."
        );
    }

}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::rendered_lines_need_no_scratch_allocations;

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use super::{display_line, excerpt};
    use crate::diagnostics::Change;

    fn render_with_panicking_callback_destructor() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("renderer destructor");
            }
        }

        let probe = PanicOnDrop;
        let change = Change {
            action: &(),
            before: &(),
            after: &(),
            changed: false,
        };
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            change.format_with(
                |()| String::from("unchanged action"),
                move |()| {
                    std::hint::black_box(&probe);
                    panic!("unchanged state must not render");
                },
            )
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"renderer destructor"));
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn rendered_lines_need_no_scratch_allocations() {
        if crate::allocation_test::run_in_child(
            "diagnostics::diff::tests::rendered_lines_need_no_scratch_allocations",
        ) {
            return;
        }
        let inputs = [
            String::new(),
            "short".to_owned(),
            "a".repeat(235),
            "界".repeat(234),
        ];
        let mut output = String::with_capacity(1_024);
        let expected = "same\nold\n\n";
        let actual = format!("same\n{}\n\n", "界".repeat(234));
        let action = String::from("changed");
        let before_snapshot = expected.to_owned();
        let change = Change {
            action: &action,
            before: &before_snapshot,
            after: &actual,
            changed: true,
        };
        let expected_change = change.format_with(Clone::clone, Clone::clone);
        // Warm panic machinery before the live-allocation comparison.
        render_with_panicking_callback_destructor();
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        for _ in 0..1_024 {
            for input in &inputs {
                output.clear();
                display_line(&mut output, '+', 10, std::hint::black_box(input));
                std::hint::black_box(&output);
            }
        }
        let after = dhat::HeapStats::get();
        for _ in 0..1_024 {
            std::hint::black_box(excerpt(expected, &actual, "expected", "actual"));
        }
        let full = dhat::HeapStats::get();
        for _ in 0..1_024 {
            let rendered = change.format_with(Clone::clone, Clone::clone);
            assert_eq!(rendered, expected_change);
            std::hint::black_box(rendered);
        }
        let changes = dhat::HeapStats::get();
        // Preserve the previous formatting shape as an allocation comparator,
        // not a second correctness oracle. Public tests check the rendered text.
        for _ in 0..1_024 {
            let mut rendered = super::action(&action.clone());
            rendered.push_str(&excerpt(
                &before_snapshot.clone(),
                &actual.clone(),
                "before",
                "after",
            ));
            assert_eq!(rendered, expected_change);
            std::hint::black_box(rendered);
        }
        let separate = dhat::HeapStats::get();
        render_with_panicking_callback_destructor();
        let cleanup = dhat::HeapStats::get();
        drop(profiler);
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;
        println!("displayed-lines=4096 scratch-blocks={blocks} scratch-bytes={bytes}");
        println!(
            "complete-excerpts=1024 blocks={} bytes={}",
            full.total_blocks - after.total_blocks,
            full.total_bytes - after.total_bytes,
        );
        println!(
            "complete-changes=1024 blocks={} bytes={}",
            changes.total_blocks - full.total_blocks,
            changes.total_bytes - full.total_bytes,
        );
        println!(
            "separate-excerpt-changes=1024 blocks={} bytes={}",
            separate.total_blocks - changes.total_blocks,
            separate.total_bytes - changes.total_bytes,
        );
        println!(
            "renderer-destructor-live before={}/{} after={}/{}",
            separate.curr_blocks, separate.curr_bytes, cleanup.curr_blocks, cleanup.curr_bytes,
        );
        assert_eq!(blocks, 0);
        assert_eq!(bytes, 0);
        assert!(
            changes.total_blocks - full.total_blocks < separate.total_blocks - changes.total_blocks,
            "appending into the action output must allocate fewer blocks"
        );
        assert_eq!(cleanup.curr_blocks, separate.curr_blocks);
        assert_eq!(cleanup.curr_bytes, separate.curr_bytes);
    }
}
