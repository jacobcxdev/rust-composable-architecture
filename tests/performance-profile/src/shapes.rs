use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rca_core::{Effect, ReduceContext};

use super::{Action, Config};

/// Finite timer qualification, not a wall-clock performance measurement.
pub(super) const TIMER_DELAY: Duration = Duration::from_millis(1);

/// Fixed effect shapes with separately checked response and ownership counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Store,
    Send,
    Future,
    Run,
    Merge,
    Concatenate,
    Sparse,
    Empty,
    Map,
    Cancellation,
    TimerAt,
    TimerAfter,
}

impl Mode {
    pub(super) fn parse(prefix: &str) -> Option<Self> {
        Some(match prefix {
            "store" => Self::Store,
            "effects" => Self::Send,
            "future-chain" => Self::Future,
            "run-chain" => Self::Run,
            "merge-dense" => Self::Merge,
            "concatenate" => Self::Concatenate,
            "merge-sparse" => Self::Sparse,
            "merge-empty" => Self::Empty,
            "map-deferred" => Self::Map,
            "cancellation-depth" => Self::Cancellation,
            "timers-at" => Self::TimerAt,
            "timers-after" => Self::TimerAfter,
            _ => return None,
        })
    }

    pub(super) const fn name(self, native: bool) -> &'static str {
        let names = match self {
            Self::Store => ("store-native", "store-deterministic"),
            Self::Send => ("effects-native", "effects-deterministic"),
            Self::Future => ("future-chain-native", "future-chain-deterministic"),
            Self::Run => ("run-chain-native", "run-chain-deterministic"),
            Self::Merge => ("merge-dense-native", "merge-dense-deterministic"),
            Self::Concatenate => ("concatenate-native", "concatenate-deterministic"),
            Self::Sparse => ("merge-sparse-native", "merge-sparse-deterministic"),
            Self::Empty => ("merge-empty-native", "merge-empty-deterministic"),
            Self::Map => ("map-deferred-native", "map-deferred-deterministic"),
            Self::Cancellation => (
                "cancellation-depth-native",
                "cancellation-depth-deterministic",
            ),
            Self::TimerAt => ("timers-at-native", "timers-at-deterministic"),
            Self::TimerAfter => ("timers-after-native", "timers-after-deterministic"),
        };
        if native { names.0 } else { names.1 }
    }

    pub(super) const fn responses(self, size: u64) -> u64 {
        match self {
            Self::Store | Self::Empty => 0,
            Self::Sparse => {
                if size == 0 {
                    0
                } else {
                    1
                }
            }
            Self::Map | Self::Cancellation => 1,
            _ => size,
        }
    }

    /// Concurrent branches must preserve identity, not a particular arrival order.
    pub(super) const fn ordered(self) -> bool {
        !matches!(self, Self::Merge | Self::TimerAt | Self::TimerAfter)
    }

    pub(super) const fn identities(self) -> bool {
        !matches!(self, Self::Store | Self::Send)
    }

    #[cfg(feature = "deterministic")]
    pub(super) const fn timer(self) -> bool {
        matches!(self, Self::TimerAt | Self::TimerAfter)
    }
}

/// Completed work and released captures, checked after causal completion and teardown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkFacts {
    future_entries: u64,
    run_entries: u64,
    send_completions: u64,
    mapper_invocations: u64,
    captures_created: u64,
    captures_dropped: u64,
    timer_registrations: u64,
}

impl WorkFacts {
    /// Derives counts from the validated input, independently of observed counters.
    pub(super) fn expected(config: Config) -> Self {
        let responses = config.reductions - config.iterations;
        let mut facts = Self::default();
        match config.mode {
            Mode::Future | Mode::Merge | Mode::Concatenate => facts.future_entries = responses,
            Mode::Run => {
                facts.run_entries = responses;
                facts.send_completions = responses;
            }
            Mode::Map => {
                facts.future_entries = config.iterations;
                facts.mapper_invocations = config.size * config.iterations;
            }
            Mode::TimerAt | Mode::TimerAfter => facts.timer_registrations = responses,
            _ => {}
        }
        facts.captures_created =
            facts.future_entries + facts.run_entries + facts.mapper_invocations;
        facts.captures_dropped = facts.captures_created;
        facts
    }

    pub(crate) fn json(self) -> String {
        format!(
            "{{\"future_entries\":{},\"run_entries\":{},\"send_completions\":{},\"mapper_invocations\":{},\"captures_created\":{},\"captures_dropped\":{},\"timer_registrations\":{}}}",
            self.future_entries,
            self.run_entries,
            self.send_completions,
            self.mapper_invocations,
            self.captures_created,
            self.captures_dropped,
            self.timer_registrations
        )
    }
}

/// Shared counters; the host's completion and teardown boundaries order final reads.
#[derive(Default)]
pub(super) struct Probes {
    future_entries: AtomicU64,
    run_entries: AtomicU64,
    send_completions: AtomicU64,
    mapper_invocations: AtomicU64,
    captures_created: AtomicU64,
    captures_dropped: AtomicU64,
    timer_registrations: AtomicU64,
}

impl Probes {
    pub(super) fn facts(&self) -> WorkFacts {
        WorkFacts {
            future_entries: self.future_entries.load(Ordering::Relaxed),
            run_entries: self.run_entries.load(Ordering::Relaxed),
            send_completions: self.send_completions.load(Ordering::Relaxed),
            mapper_invocations: self.mapper_invocations.load(Ordering::Relaxed),
            captures_created: self.captures_created.load(Ordering::Relaxed),
            captures_dropped: self.captures_dropped.load(Ordering::Relaxed),
            timer_registrations: self.timer_registrations.load(Ordering::Relaxed),
        }
    }
}

/// Keeps the reducer owner alive until an effect or mapper releases its capture.
struct Capture {
    probes: Arc<Probes>,
    _owner: Arc<()>,
}

impl Capture {
    fn new(probes: &Arc<Probes>, owner: &Arc<()>) -> Self {
        probes.captures_created.fetch_add(1, Ordering::Relaxed);
        Self {
            probes: Arc::clone(probes),
            _owner: Arc::clone(owner),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.probes.captures_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts a real future entry and releases its witness before returning the action.
fn future(action: Action, probes: &Arc<Probes>, owner: &Arc<()>) -> Effect<Action> {
    let capture = Capture::new(probes, owner);
    Effect::future(async move {
        capture
            .probes
            .future_entries
            .fetch_add(1, Ordering::Relaxed);
        drop(capture);
        Some(action)
    })
}

/// Builds only the selected shape; its probes remain included in measured work.
pub(super) fn effect(
    config: Config,
    action: &Action,
    context: &ReduceContext<'_>,
    probes: &Arc<Probes>,
    owner: &Arc<()>,
) -> Effect<Action> {
    let response = |offset| Action {
        value: action.value + offset,
        remaining: 0,
        root: false,
        mapped: 0,
    };
    match config.mode {
        Mode::Store => Effect::none(),
        Mode::Send | Mode::Future | Mode::Run => {
            if action.remaining == 0 {
                return Effect::none();
            }
            let next = Action {
                remaining: action.remaining - 1,
                ..response(1)
            };
            match config.mode {
                Mode::Send => Effect::send(next),
                Mode::Future => future(next, probes, owner),
                Mode::Run => {
                    let capture = Capture::new(probes, owner);
                    Effect::run(move |sender| async move {
                        capture.probes.run_entries.fetch_add(1, Ordering::Relaxed);
                        if sender.send(next).await.is_ok() {
                            capture
                                .probes
                                .send_completions
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        drop(capture);
                    })
                }
                _ => unreachable!(),
            }
        }
        _ if !action.root => Effect::none(),
        Mode::Merge => {
            Effect::merge((1..=config.size).map(|offset| future(response(offset), probes, owner)))
        }
        Mode::Concatenate => Effect::concatenate(
            (1..=config.size).map(|offset| future(response(offset), probes, owner)),
        ),
        Mode::Sparse => Effect::merge((0..config.size).map(|index| {
            if index == 0 {
                Effect::send(response(1))
            } else {
                Effect::none()
            }
        })),
        Mode::Empty => Effect::merge((0..config.size).map(|_| Effect::none())),
        Mode::Map => {
            let mut effect = future(response(1), probes, owner);
            for _ in 0..config.size {
                let capture = Capture::new(probes, owner);
                effect = effect.map(move |mut output| {
                    capture
                        .probes
                        .mapper_invocations
                        .fetch_add(1, Ordering::Relaxed);
                    output.mapped += 1;
                    output
                });
            }
            effect
        }
        Mode::Cancellation => {
            let mut effect = Effect::send(response(1));
            for depth in 0..config.size {
                effect = effect.cancellable(depth);
            }
            effect
        }
        Mode::TimerAt | Mode::TimerAfter => {
            let deadline = context.now().saturating_add(TIMER_DELAY);
            Effect::merge((1..=config.size).map(|offset| {
                // Every submitted program is an actor-owned one-shot registration;
                // exact delivery and causal completion certify all registrations.
                probes.timer_registrations.fetch_add(1, Ordering::Relaxed);
                if config.mode == Mode::TimerAt {
                    Effect::send_at(deadline, response(offset))
                } else {
                    Effect::send_after(TIMER_DELAY, response(offset))
                }
            }))
        }
    }
}
