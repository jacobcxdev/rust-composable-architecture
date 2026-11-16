//! Runs finite Task and fold costs with explicit measurement and reporting phases.
//!
//! Use `task_fold_cost WORKLOAD SIZE ITERATIONS 1`. The controller sends GO,
//! REPORT and EXIT after READY, DONE and the JSON record, respectively.
//! The measured batch includes construction, work, cleanup and host disposal.
//! JSON encoding is outside that interval. This executable makes no speed claim.

#[path = "../task_fold_cost.rs"]
mod workloads;

use std::error::Error;
use std::io::{self, BufRead, Write};
use std::time::Instant;

/// Returns protocol, input and workload failures through one application channel.
type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Describes rejected external input without an internal contract panic.
fn invalid(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    io::Error::other(message.into()).into()
}

/// Rejects EOF and out-of-order controller commands.
fn expect_command(input: &mut impl BufRead, expected: &str) -> Result<()> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 || line.trim() != expected {
        return Err(invalid(format!("expected {expected} controller command")));
    }
    Ok(())
}

/// Reports only checked work, then requires the controller's final acknowledgement.
fn main() -> Result<()> {
    let config = workloads::Config::parse(std::env::args().skip(1))?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    writeln!(output, "READY")?;
    output.flush()?;
    expect_command(&mut input, "GO")?;

    let start = Instant::now();
    let result = workloads::profile_task_fold_work(&config)?;
    let elapsed = u64::try_from(start.elapsed().as_nanos())?;
    writeln!(output, "DONE")?;
    output.flush()?;
    expect_command(&mut input, "REPORT")?;

    let (schema, clock) = if cfg!(feature = "task-fold-controlled") {
        ("rca-task-fold-controlled-cost-v1", "paused-checked")
    } else {
        ("rca-task-fold-cost-v1", "live")
    };
    // Names come only from a fixed ASCII list. Every other field is a scalar.
    writeln!(
        output,
        concat!(
            "{{\"schema\":\"{}\",\"workload\":\"{}\",",
            "\"size\":{},\"iterations\":{},\"workers\":1,\"operations\":{},",
            "\"checksum\":{},\"elapsed_ns\":{},\"latencies_ns\":[],",
            "\"cleanup_complete\":true,\"details\":{{\"allocator\":\"system\",",
            "\"runtime\":\"Tokio current-thread\",\"task_groups_enabled\":{},",
            "\"clock\":\"{}\",",
            "\"boundary\":\"profile_task_fold_work\",",
            "\"scope\":\"construction, checked work, shutdown and host disposal\"}}}}"
        ),
        schema,
        config.name(),
        config.size,
        config.iterations,
        result.operations,
        result.checksum,
        elapsed,
        cfg!(feature = "task-fold-groups"),
        clock
    )?;
    output.flush()?;
    expect_command(&mut input, "EXIT")
}
