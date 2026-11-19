# Debounce and planned-throttle correctness fixtures

Two independent, locked workspaces exercise current RCA and upstream Composable
`0b5baa80246a625c1bb886e6df20dae27fc97c97`. These are virtual-time correctness
tests, not live-clock tests or a timing programme.

Run `cargo test --locked --manifest-path tests/composable-timers/candidate/Cargo.toml`
and substitute `upstream`. Apply an external process deadline
(CI uses two minutes): the legacy native `TestStore::wait` is unbounded.

The shared literal timeline uses a four-second trailing debounce:

- Input 1 at time zero; no output at time 3.
- Input 2 at time 3 replaces input 1. No output at the original deadline, time 4,
  or one nanosecond before time 7. Exactly output 2 is received at time 7.
- Input 3 at time 7 starts another cycle. No output one nanosecond before time
  11; exactly output 3 is received at time 11.
- Advance another 20 seconds and check for extra outputs. Final domain trace
  must be exactly `[2, 3]`.

Legacy adapters invoke native `Scheduler::debounce`, retaining its native `Task`
in reducer state. Their assertion snapshots omit that handle, following the
pinned `TestClock` example, and compare domain data only. `TestClock::advance`
drives native `Delay`/`Reactor` work but queues output until `TestStore::recv`.
Each no-output checkpoint sends a no-op action: native `send` rejects unreceived
output before reducing that action. After the final advance, native `wait`
exhausts scheduled tasks, another checkpoint rejects remaining output, and
`into_inner` checks final state. This is test-executor exhaustion, not joining a
live Store host or proving timer-reactor resource reclamation.

RCA uses `Effect::send(...).delay(...).cancellable_in_flight("debounce")` with
`rca_test::TestStore`. Its native no-op `send` also rejects pending output;
`receive_with` verifies each exact output and state change. `finish` verifies
exhaustive completion. Dropped action observation handles do not cancel work.
The fixture equates the specified observable timeline, not the engines' task
ownership, queueing, clock-origin or teardown contracts.

Each engine runs two negative controls through those same adapters: omit
cancellation, or shorten the deadline by one nanosecond. Both must fail at a
native no-output checkpoint. Controls run in subprocesses with ten-second
deadlines and require the expected diagnostic, not merely unsuccessful exit.
The ignored `mutation_child` test is invoked by those subprocesses; it is not
missing coverage. Legacy assertion failures can also panic during TestStore
destruction, so subprocess isolation contains that baseline behaviour.

The shared subprocess runner retains at most one MiB of stderr plus one byte to
detect overflow. It drains excess output before reporting the limit failure.
A timeout or process-status error triggers kill and reap attempts before the
reader joins. Children must not pass stderr writers to surviving descendants.
Separate runner controls reject success, unrelated diagnostics, oversized output
and timeout. The process deadline is a failure bound, not a performance metric.

The debounce trace does not establish live timer accuracy, repeating intervals, throttle,
leading debounce, simultaneous-deadline ordering, queued-output revocation,
general cancellation equivalence, allocation costs or comparative performance.

## Trailing throttle

The same two workspaces compare native `Scheduler::throttle` with an RCA
recipe using a stored `Option<MonotonicInstant>`, `Effect::send_at` and scoped
cancel-in-flight. No core operator, scheduler or runtime state was added.
The period is four seconds. The shared literal timeline is:

| Absolute time | Operation | Expected result |
| --- | --- | --- |
| 0 | Input 1 | Due 4; no immediate output |
| 3 | Input 2 | Replaces 1, still due 4 |
| 4 − 1 ns | Check | No output |
| 4 | Receive 2, then input 3 | Next deadline 8 |
| 5 | Input 4 | Replaces 3, still due 8 |
| 8 − 1 ns / 8 | Check / receive | No early output / output 4 |
| 9 | Input 8 | Due 12 from retained deadline 8, not 13 |
| 12 − 1 ns / 12 | Check / receive | No early output / output 8 |
| 16 | Input 5 | Exact end of recent window: new trailing deadline 20 |
| 20 − 1 ns / 20 | Check / receive | No early output / output 5 |
| 30 | Input 6 | Idle restart, due 34 |
| 31 | Cancel | Cancel pending output and clear scheduling history |
| 34 | Check | No cancelled output, before another input can replace it |
| 35 | Input 7 | Fresh deadline 39, not retained-history deadline 38 |
| 38 / 39 − 1 ns / 39 | Check / check / receive | No early output / output 7 |
| 59 | Check, exhaust | No extras; final trace `[2, 4, 8, 5, 7]` |

A second fresh-store trace observes output 1 after advancing directly from zero
to six seconds, then inputs 2. Output 2 is due at eight, based on the previous
planned deadline four, not ten based on observation time six. The test checks
one nanosecond before eight, receives exactly output 2 at eight, then advances
another twenty seconds and exhausts. It does not equate internal wake timing.

Native adapters retain `Option<Task>` after normal output. Their snapshot clones
omit the task, so clock metadata is checked through output timing, not state
equality. Explicit cancellation consumes the native task and clears that option.
RCA retains a plain deadline after normal output and clears it while cancelling
the effect identity. Candidate `send_with` assertions also check literal planned
deadlines. Compared with the native one-call helper, RCA requires the three-way
deadline selection in its reducer; this is a caller-code cost, not API-ergonomic
parity. No allocation or binary-size comparison has been measured here.

Three throttle mutation children must fail with relevant native diagnostics:
leading-first delivery, missing replacement cancellation, and missing explicit
reset cancellation. The second mutation detaches the old native task (also
discarding its history), while RCA keeps the deadline but omits its cancellation
wrapper. These are stale-output controls, not identical ownership mutations.
The third detaches/omits cancellation at reset and fails at time 34 before any
new input. Subprocesses retain the ten-second deadline and reject timeout as
evidence. Together with debounce, each engine executes five mutation children.

This qualifies only the specified finite virtual-time action schedules. Boundary
outputs are received before new boundary inputs. It does not prove queued-output
revocation equivalence, generic effect throttling, scoped-identity isolation,
overflow/MAX behaviour, native timer precision or comparative performance.

## Leading debounce

The two workspaces also compare the pinned engines' leading debounce policy.
This is fixed-window suppression: an accepted input emits at the current clock
instant and starts a four-second window. Inputs through its inclusive end are
dropped without moving that window. It is not a resetting quiet-period policy.

| Absolute time | Operation | Expected output |
| --- | --- | --- |
| 0 | Input 1, then input 2 after receiving 1 | 1 only |
| 3 | Input 3 | None; original window stays fixed |
| 4 | Input 4 | None; end boundary is inclusive |
| 4 s + 1 ns | Input 5 | 5 |
| 7 | Input 6 | None; new window stays fixed |
| 8 s + 1 ns | Input 7 | None; second inclusive end |
| 8 s + 2 ns | Input 8 | 8 |
| 9 | Reset, then input 9 | 9 despite previous window |
| 9 | Input 10 after receiving 9 | None |
| 20 | Input 11 | 11 after idle restart |
| 40 | Check and exhaust | No extras; trace `[1, 5, 8, 9, 11]` |

Every suppressed input has an explicit no-output checkpoint. Legacy adapters
call native `Scheduler::debounce(Interval::Leading(...))` and retain its
`Option<Task>` after output. They drive `advance(Duration::ZERO)` after every
input: native `send` reduces but does not itself poll the immediate timer.
RCA's `TestStore::send` already drives immediate effects; advancing again with
unreceived output is correctly rejected.

The RCA recipe stores the last accepted `MonotonicInstant`, suppresses through
`previous + period`, and otherwise uses `send_at(now, ...)` with scoped
cancel-in-flight. Normal output preserves history. Reset clears it and cancels
the identity; the native recipe consumes its task. RCA explicitly asserts each
literal timestamp as well as output state. This is additional caller code
compared with the native helper, not API-ergonomic parity. No core operator was
added. The scalar comparison does not impose a Clone requirement on RCA actions.

Three bounded mutation subprocesses per engine catch an open end boundary,
history cleared on output, and history not cleared on reset. Legacy open-boundary
control shortens its period by one nanosecond; RCA changes `<=` to `<`.
Candidate controls use literal mutation-state assertions so state differences
do not hide the intended output failures. A timeout is never accepted as a
successful control. Each full engine suite now executes eight mutation children.

All accepted outputs are received before Reset. This proves reset of completed
scheduling history, not cancellation or revocation of pending/queued output.
It does not qualify clock overflow/MAX, zero periods, native timer precision,
generic effect debounce, simultaneous producer ordering, allocation costs or
comparative performance.
