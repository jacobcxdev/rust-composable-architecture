# Captured MCMaitreD test coverage

Historical local source checkpoint: `1756b8eca556d27bc898ed5aca12c23869c6a312`.
This inventory maps all 24 captured server tests to RCA example tests.
It does not declare complete migration or compare performance.
Checkpoint revisions, original captures and local logs below are retained
separately, not distributed as public reproduction inputs. See the
[qualification summary](../qualification.md) for the current candidate boundary.

The separately retained local source capture remains the historical baseline.
Its unchanged workspace failed compilation. The separate leaf-only patch disabled
the unused routing adapter and retains the original domain tests.
Neither that exclusion nor the current keyed example repairs the captured parent adapter.

Current test names below belong to `examples/mcmd-server/tests/`:

- B: [backend.rs](../../examples/mcmd-server/tests/backend.rs).
- P: [polling.rs](../../examples/mcmd-server/tests/polling.rs).
- X: [proxy.rs](../../examples/mcmd-server/tests/proxy.rs).
- S: [server.rs](../../examples/mcmd-server/tests/server.rs).
- C: [server_proxy.rs](../../examples/mcmd-server/tests/server_proxy.rs).
- M: [proxy_mailbox.rs](../../examples/mcmd-server/tests/proxy_mailbox.rs).
- G: [managed_proxy.rs](../../examples/mcmd-server/tests/managed_proxy.rs).
- O: [managed_proxy_owner.rs](../../examples/mcmd-server/tests/managed_proxy_owner.rs).
- K: [managed_servers.rs](../../examples/mcmd-server/tests/managed_servers.rs).

“Retained” means that the named domain outcome has executable coverage.
It does not mean identical action names, state layout or intermediate scheduling.
“Changed” identifies a deliberate representation or policy change.
“Open” identifies captured behaviour without a qualified replacement.

## Complete captured test inventory

| Captured test | Current evidence | Disposition |
| --- | --- | --- |
| `request_start_sets_target_and_enqueues_backend_start` | B `start_success_emits_started` | Retained target update and subsequent start. `Reconcile` replaces `BackendStart`. |
| `backend_start_transitions_to_starting_and_calls_client` | B `start_success_emits_started`; S `wanted_start_polls_and_confirmed_stop_resets` | Retained pending phase, exact client identity, success and polling handoff. |
| `backend_start_noops_when_target_stopped` | B `stable_and_error_reconciliation_is_idle` | Retained stopped/stopped guard and zero client calls. |
| `backend_start_noops_when_not_stopped` | B `stable_and_error_reconciliation_is_idle` | Retained tested running/running guard. The captured test does not cover every non-stopped phase. |
| `backend_start_noops_when_error` | B `stable_and_error_reconciliation_is_idle` | Retained error/running guard. |
| `backend_start_finished_ok_when_target_stopped_enqueues_backend_stop` | B `stop_during_start_waits_then_stops_without_started_delegate`; S `unwanted_start_never_polls` | Changed intermediate phases. Completed start precedes one stop, without polling. |
| `backend_start_finished_err_sets_error_phase` | B `start_failure_preserves_error_after_target_reversal` | Retained failure and message. Dedicated start error and Debug-formatted text changed. |
| `request_stop_sets_target_and_enqueues_backend_stop` | B `stop_success_emits_stopped` | Retained target update and subsequent stop through `Reconcile`. |
| `backend_stop_noops_when_already_stopped` | B `stable_and_error_reconciliation_is_idle` | Retained stopped/stopped guard. |
| `backend_stop_noops_when_error` | B `stable_and_error_reconciliation_is_idle` | Retained error/stopped guard. |
| `backend_stop_transitions_to_stopping_and_calls_client` | B `stop_success_emits_stopped`; S `wanted_start_polls_and_confirmed_stop_resets` | Retained client identity, stopping phase and successful reset. Parent coordinates polling. |
| `backend_stop_finished_err_sets_error_phase` | B `stop_failure_preserves_error_after_target_reversal` | Retained failure and message. Dedicated stop error and Debug-formatted text changed. |
| `status_polling_start_leading_tick_polls_when_running` | P `leading_poll_and_five_second_cadence`; S `wanted_start_polls_and_confirmed_stop_resets` | Retained immediate poll after wanted start. |
| `status_polling_tick_repeats_after_interval` | P `leading_poll_and_five_second_cadence` | Retained five-second cadence. The test also rejects delivery at four seconds. |
| `status_poll_tick_ignored_on_generation_mismatch` | P `stop_preserves_result_and_prevents_future_ticks`; `stop_drops_pending_query_and_restart_uses_new_lifetime` | Changed to runtime-owned lifetimes. No integer generation or equivalent forged-tick API remains. |
| `status_poll_calls_query_and_status_polled_updates_player_count` | P `leading_poll_and_five_second_cadence` | Retained exact client identity and successful count update. |
| `status_polled_err_increments_failures_and_sets_last_error` | P `typed_errors_saturate_and_success_clears_failure_state` | Retained connection failure diagnostic and increment. Additional categories, saturation and recovery have coverage. |
| `proxy_start_noops_when_already_enabled` | X `ready_precedes_events_and_stop_releases_session`; `pending_acquisition_cancels_and_restart_is_fresh` | Retained duplicate suppression while starting and listening. |
| `proxy_start_emits_proxy_started_and_forwards_events` | X `ready_precedes_events_and_stop_releases_session` | Retained Ready-before-event order. Acquisition returns an owned session. |
| `proxy_event_ignored_on_generation_mismatch` | M `stop_invalidates_real_old_event_already_admitted_to_mailbox`; X `pending_acquisition_cancels_and_restart_is_fresh` | Changed to runtime provenance. Queued old output is rejected before reduction, not through an action's integer generation. |
| `proxy_started_err_disables_proxy_and_sets_error` | X `failure_releases_lease_and_explicit_retry_clears_error`; C `proxy_failure_and_eof_preserve_backend` | Retained failure and explicit retry. Acquisition preserves `StartFailed` and `Other` categories, payloads and Debug-shaped diagnostic text. |
| `proxy_stop_noops_when_disabled` | X `inactive_actions_do_not_acquire` | Retained inactive Stop with no client calls or resource acquisition. |
| `proxy_stop_calls_client_and_emits_proxy_stopped` | G `stop_waits_for_release_and_typed_failure_allows_explicit_retry`; `eof_requests_cleanup_instead_of_claiming_release`; K `failed_stop_blocks_removal_until_retry_then_restore_reuses_owner` | Retained exact stop-by-ID call, pending phase and success only after completion. The separate managed keyed composition also prevents removal before confirmed cleanup and restores the same owner. |
| `proxy_stopped_err_sets_last_proxy_error` | G `stop_waits_for_release_and_typed_failure_allows_explicit_retry`; K `closed_store_does_not_discard_external_cleanup_failure` | Retained typed `StopFailed` payload and Debug-shaped diagnostic. Explicit retry reconciles retained ownership, including recovery after Store closure. |

## Consequences for migration

The [backend disposition](composable-consumer.md#preserved-and-deliberately-changed-behaviour)
explains serial operation policy and error representation changes.
The [polling checkpoint](composable-consumer.md) records one in-flight query and
cancellation of stale work. These changes need explicit comparison, not literal
replays of obsolete internal actions.

The local proxy contract suits resources that release synchronously on Drop.
The [socket receipt](native-proxy-ownership.md) supplies actual local EOF evidence.
It does not support fallible asynchronous cleanup or prove remote release.
The separate managed proxy demonstrates independently owned start/stop operations
and cancellable RCA replies. G covers completed cleanup, exact failure/retry,
EOF cleanup, failed acquisition, restart and reply cancellation. O covers bounded
admission, serialisation, recovery, failed partial acquisition and abandoned
replies. K composes backend, polling and managed proxy lifetimes with a fixed
keyed roster, sibling isolation and removal/reinsertion after confirmed cleanup.
These narrow the two captured stop rows to controlled-client retention; they do
not establish whole-application migration or native cleanup equivalence.

The [managed local TCP receipt](native-managed-proxy-ownership.md) additionally
checks delayed release, failed stop, reply cancellation and recovery against
independent peer EOF. This is a test-only adapter, not a production proxy or
general remote cleanup guarantee.

The [asynchronous owner fixture](../../rca-tokio/tests/async_cleanup_host.rs)
qualifies a prerequisite independently: an application-owned task retains a
controlled stop result after its RCA reply effect is cancelled or its Store shuts
down. Admission does not transition state to Stopped; failure remains distinct
from confirmed release. Wrong-session, full-queue and closed-host paths reject
explicitly. This is not a migrated proxy client, a native cleanup adapter or a
recovery policy. The separate managed consumer adds acquisition/stop ownership,
in-memory recovery and explicit retry. The later managed keyed composition adds
server integration. Durable ownership, production native clients and daemon
integration remain outside the qualified result.

Configuration validation has separate tests, outside these 24 captured server tests.
Input loading, daemon orchestration and deployment remain outside this inventory.
The captured daemon entry point is empty, so those features are not migrated
behaviour that this inventory can claim to preserve.

## Earlier execution checkpoint

Both suites passed on aarch64 macOS with explicit Rust/Cargo/Rustdoc 1.98.0
paths, cleared compiler wrappers and flags, and the existing isolated target.
The current command was `cargo test --offline --locked -p rca-example-mcmd-server`.
All 58 tests passed, with no failures or ignores.

The baseline command was `cargo test --offline --locked --workspace` in the
existing leaf-only extraction, `/tmp/rca-mcmd-leaf-baseline.hETi6R`.
All 24 original tests passed, with no failures or ignores.
The daemon and doc-test targets contain no tests.
Four existing dead-code warnings concern floor/table configuration and validation.

The initial combined baseline build/test hit its external 120-second deadline
during compilation, after waiting for the shared build lock. It ran no tests.
A separate `--no-run` build completed within its 300-second deadline.
The subsequent test run passed within its 60-second deadline.
These are correctness checks, not compile-time measurements.

The server source, backend/proxy client source and root lockfile matched their
recorded archive hashes. A reverse-apply check accepted the existing leaf-only
patch without changing files. The 24 table names match the captured test names
exactly. This inventory check does not establish behavioural equivalence by itself.
No Rust source, original consumer file or baseline file changed in this checkpoint.

| Local evidence | SHA-256 |
| --- | --- |
| `/tmp/rca-mcmd-coverage-current.log` | `0193f5db4b271a9e56773ffacac6ccd1162be2343ddcf6f0f90b1d2fc5b4b7f0` |
| `/tmp/rca-mcmd-coverage-baseline.log` (timed out) | `411d087b173fa1a9efb59a1d93ed88dcf370db923ff02a7710df5f1eaa374ea5` |
| `/tmp/rca-mcmd-coverage-baseline-build.log` | `ac3048f377d0b6b7c80ba6c889b27cbf224dc58885a3ab02fdb5fea13957a27d` |
| `/tmp/rca-mcmd-coverage-baseline-tests.log` | `ee450bda29065bc7a25bdc0630d87a3e699f03b529d53b6d2d2c1bd275b64f5a` |

This is an author-reviewed coverage map, not independent migration approval.
At that checkpoint the two asynchronous proxy-stop cases remained open despite
both suites passing. The managed consumer above adds separate executable evidence;
it does not retroactively broaden that earlier receipt.

## Managed-consumer execution checkpoint

The [managed integration receipt](composable-consumer.md#managed-external-proxy-integration)
records 73 passing consumer tests within the all-feature workspace run on pinned
Rust 1.98.0/aarch64 macOS, strict package Clippy and fresh independent review.
Fifteen tests qualify the new owner/reducer contract. The two captured stop
outcomes have controlled-client replacements; native external release and
whole-server/keyed adoption were unqualified at that checkpoint. The later
[managed keyed checkpoint](composable-consumer.md#managed-ownership-in-keyed-server-composition) adds six
composed traces; it does not retroactively broaden the earlier run.
