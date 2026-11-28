# Read macOS build counters

A compiler invocation has several costs. Choose the counter that answers the question:

| Question | Counter from `/usr/bin/time -lp` | Limit |
| --- | --- | --- |
| How long did the caller wait? | Elapsed wall time | Includes time in queues and time spent waiting for the operating system. |
| How much CPU time did the build use? | User and system seconds | Includes the command and children it waited for. Keep user and system time separate. |
| How large was the largest resident process? | Maximum resident set size, in bytes | Not the sum of memory in processes that ran together. |

Even with one Cargo build job, Cargo and rustc can run together. Neither maximum RSS nor maximum footprint measures their combined peak memory.
A detached compiler or cache daemon can escape command accounting. The [memory watchdog](../../CONTRIBUTING.md#build-memory-safety) serves a different purpose: it limits local build risk.

Compare the same command and build state on both sides. An empty Cargo target excludes previous compiler output, not the operating system's file cache.
For a clean build, start each sample from an empty target. For an incremental build, specify both the edit and the starting target.
Keep the completed command, exit status, and raw counters with the result. If a printed value rounds to zero, do not substitute a small positive number to calculate a ratio.
