//! Finite normal-allocator naming and fold controls on the existing native host.
//!
//! Every interval includes construction, checked work and shutdown. Fold reads
//! repeat on one built shape. Churn includes removal and reinsertion.
//! These are native batch costs, not isolated instruction-deterministic polls.

use std::cell::Cell;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rca_core::{FoldKey, KeyedState, Runtime};

use crate::{Result, invalid};

/// Holds checked dimensions for one finite native batch.
#[derive(Debug)]
pub(crate) struct Config {
    /// Selects the carrier and exact workload.
    case: Case,
    /// Counts names or rows per group, except the explicit empty sentinel.
    pub(crate) size: usize,
    /// Counts completed iterations without timing-based termination.
    pub(crate) iterations: u64,
}

impl Config {
    /// Rejects unsupported graphs, dimensions and scalar overflow before READY.
    ///
    /// # Errors
    /// Returns an error for malformed input, unsupported features or excessive work.
    pub(crate) fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        if cfg!(any(
            feature = "deterministic",
            feature = "diagnostics",
            feature = "allocations",
            feature = "persistence",
            feature = "local"
        )) {
            return Err(invalid(
                "task/fold costs require task-fold without other profiling features",
            ));
        }
        let mut args = args;
        let name = args.next().ok_or_else(|| invalid("missing WORKLOAD"))?;
        let case = Case::parse(&name).ok_or_else(|| invalid("unknown WORKLOAD"))?;
        let mut number = || -> Result<u64> {
            let value = args
                .next()
                .ok_or_else(|| invalid("expected SIZE ITERATIONS WORKERS"))?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("dimensions must be positive decimal integers"));
            }
            let value = value.parse::<u64>()?;
            if value == 0 {
                return Err(invalid("dimensions must be positive"));
            }
            Ok(value)
        };
        let size = number()?;
        let iterations = number()?;
        let workers = number()?;
        if workers != 1 || args.next().is_some() {
            return Err(invalid(
                "require exactly WORKLOAD SIZE ITERATIONS WORKERS, with WORKERS=1",
            ));
        }
        case.validate_size(size)?;
        let limit = u128::from(i64::MAX.unsigned_abs());
        if case.operations_per_iteration() * u128::from(iterations) > limit
            || case.checksum_per_iteration(size) * u128::from(iterations) > limit
        {
            return Err(invalid("operations or checksum exceeds Int64"));
        }
        Ok(Self {
            case,
            size: usize::try_from(size)?,
            iterations,
        })
    }

    /// Returns the admitted, fixed workload name for JSON reporting.
    pub(crate) const fn name(&self) -> &'static str {
        self.case.name
    }
}

/// Contains observed results only after every work and cleanup assertion passes.
#[derive(Debug, Default)]
pub(crate) struct WorkResult {
    /// Counts completed reads, task bodies or churn operations.
    pub(crate) operations: u64,
    /// Contains the sum from actual committed state and fold reads.
    pub(crate) checksum: u64,
}

/// Rejects cancellation instead of treating it as successful completion.
async fn finish_success(task: rca_core::ActionTask) {
    task.finish().await;
    assert!(task.is_finished(), "caused work finished");
    assert!(!task.is_cancelled(), "caused work was not cancelled");
}

/// Names the actual carrier and operation, including explicit empty shapes.
const NAMES: &[&str] = &[
    "task-unnamed-shared",
    "task-named-shared",
    "task-unnamed-local",
    "task-named-local",
    "fold-root-empty-shared",
    "fold-root-shared",
    "fold-root-sparse-shared",
    "fold-subtree-empty-shared",
    "fold-subtree-shared",
    "fold-subtree-sparse-shared",
    "fold-root-churn-shared",
    "fold-subtree-churn-shared",
    "fold-root-empty-local",
    "fold-root-local",
    "fold-root-sparse-local",
    "fold-subtree-empty-local",
    "fold-subtree-local",
    "fold-subtree-sparse-local",
    "fold-root-churn-local",
    "fold-subtree-churn-local",
];

/// Separates naming depth from roster shape without changing the wire protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// Runs one typed body with optional operation names.
    Task {
        /// Uses SIZE names rather than the unnamed path.
        named: bool,
    },
    /// Reads the fixed three-group tree, with optional roster changes.
    Fold {
        /// Selects only the middle group rather than the root.
        subtree: bool,
        /// Registers only row IDs divisible by sixteen.
        sparse: bool,
        /// Builds groups without rows, regardless of the SIZE sentinel.
        empty: bool,
        /// Removes and restores the middle group's first row each iteration.
        churn: bool,
    },
}

/// Contains a recognised finite case with separately checked dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Case {
    /// Uses one fixed ASCII name for argument and result admission.
    name: &'static str,
    /// Keeps actors and tasks on their owning thread.
    local: bool,
    /// Selects the body and its arithmetic contract.
    kind: Kind,
}

impl Case {
    /// Resolves exact names, never silently substituting a different shape.
    fn parse(name: &str) -> Option<Self> {
        let name = NAMES.iter().copied().find(|known| *known == name)?;
        let local = name.ends_with("-local");
        let stem = name.strip_suffix(if local { "-local" } else { "-shared" })?;
        let kind = match stem {
            "task-unnamed" => Kind::Task { named: false },
            "task-named" => Kind::Task { named: true },
            _ => Kind::Fold {
                subtree: stem.starts_with("fold-subtree"),
                sparse: stem.ends_with("-sparse"),
                empty: stem.ends_with("-empty"),
                churn: stem.ends_with("-churn"),
            },
        };
        Some(Self { name, local, kind })
    }

    /// Keeps original-API rows buildable against the immutable baseline.
    fn validate_size(self, size: u64) -> Result<()> {
        match self.kind {
            Kind::Task { named: false } | Kind::Fold { empty: true, .. } if size != 1 => {
                Err(invalid("unnamed and explicit empty cases require SIZE=1"))
            }
            Kind::Task { named: true } if !(1..=3).contains(&size) => {
                Err(invalid("named Task SIZE is exactly 1, 2 or 3 names"))
            }
            Kind::Task { named: true } if size > 1 && !cfg!(feature = "task-fold-groups") => {
                Err(invalid("nested Task names require task-fold-groups"))
            }
            Kind::Fold { empty: false, .. } if ![1, 3, 16, 256, 1024].contains(&size) => Err(
                invalid("fold SIZE must be 1, 3, 16, 256 or 1024 rows per group"),
            ),
            _ => Ok(()),
        }
    }

    /// Counts read completions and, for churn, the two committed roster changes.
    const fn operations_per_iteration(self) -> u128 {
        if matches!(self.kind, Kind::Fold { churn: true, .. }) {
            4
        } else {
            1
        }
    }

    /// Gives an independent scalar oracle for three groups valued one/two/three.
    fn checksum_per_iteration(self, size: u64) -> u128 {
        match self.kind {
            Kind::Task { .. } => 1,
            Kind::Fold { empty: true, .. } => 0,
            Kind::Fold {
                subtree,
                sparse,
                churn,
                ..
            } => {
                let contributors = if sparse { (size - 1) / 16 + 1 } else { size };
                let sum = u128::from(contributors) * if subtree { 2 } else { 6 };
                if churn { 2 * sum - 2 } else { sum }
            }
        }
    }
}

/// Keeps the result non-`Sync` to exercise synchronous ownership.
struct SumValue(Cell<u64>);
/// Combines contribution values without allocating an accumulator.
struct Sum;

impl FoldKey for Sum {
    type Value = SumValue;

    fn identity() -> SumValue {
        SumValue(Cell::new(0))
    }

    fn reduce(acc: SumValue, next: SumValue) -> ControlFlow<SumValue, SumValue> {
        acc.0.set(acc.0.get() + next.0.get());
        ControlFlow::Continue(acc)
    }
}

/// Uses one source body for both public carriers, preserving their ownership types.
macro_rules! carrier {
    ($local:expr) => {
        use super::{
            Arc, AtomicU64, Case, Cell, Config, KeyedState, Kind, Ordering, Result, Sum, SumValue,
            WorkResult, finish_success,
        };
        use domain::{
            EntryMount, Feature, FeatureContext, Fold, MountChildren, OptionalMount, Store, Task,
        };

        /// Declares the same typed body at every naming depth.
        struct TaskFeature {
            /// Counts registered operation names, including the first old-API name.
            names: usize,
            /// Remains owned until the feature and its task captures are released.
            owner: Owner<()>,
            /// Counts actual body starts rather than declarations.
            entered: Arc<AtomicU64>,
            /// Counts bodies after their typed updates commit.
            finished: Arc<AtomicU64>,
        }

        impl Feature for TaskFeature {
            type State = u64;
            type Action = ();

            fn update(&mut self, _: &mut u64, _: &(), context: &mut FeatureContext<'_, u64, ()>) {
                let owner = Owner::clone(&self.owner);
                let entered = Arc::clone(&self.entered);
                let finished = Arc::clone(&self.finished);
                let thread = if $local {
                    Some(std::thread::current().id())
                } else {
                    None
                };
                let run = move |context: domain::TaskContext<u64, ()>| async move {
                    entered.fetch_add(1, Ordering::Relaxed);
                    // One real suspension is identical at every naming depth.
                    tokio::task::yield_now().await;
                    if $local {
                        assert_eq!(
                            Some(std::thread::current().id()),
                            thread,
                            "Local task affinity"
                        );
                    }
                    drop(
                        context
                            .update(|state: &mut u64| *state += 1)
                            .await
                            .expect("live typed task update commits"),
                    );
                    finished.fetch_add(1, Ordering::Relaxed);
                    drop(owner);
                };
                let task = if self.names == 0 {
                    Task::run(run)
                } else {
                    // Old public API: baseline and candidate execute the same one-name body.
                    Task::run_named(0_usize, rca_core::OperationPolicy::Coexist, run)
                };
                #[cfg(feature = "task-fold-groups")]
                let task = {
                    let mut task = task;
                    for key in 1..self.names {
                        task = task.named(key, rca_core::OperationPolicy::Coexist);
                    }
                    task
                };
                context.task(task);
            }
        }

        /// Repeats complete typed bodies without status polling.
        pub(super) async fn tasks(
            config: &Config,
            names: usize,
            runtime: domain::Runtime,
        ) -> Result<WorkResult> {
            let owner = Owner::new(());
            let released = Owner::downgrade(&owner);
            let entered = Arc::new(AtomicU64::new(0));
            let finished = Arc::new(AtomicU64::new(0));
            let store = Store::new(
                0_u64,
                TaskFeature {
                    names,
                    owner,
                    entered: Arc::clone(&entered),
                    finished: Arc::clone(&finished),
                },
                runtime,
            );
            store.ready().await?;
            let mut operations = 0;
            for _ in 0..config.iterations {
                finish_success(store.send(()).await?).await;
                operations += 1;
            }
            let checksum = store.read(|state| *state).await?;
            assert_eq!(entered.load(Ordering::Relaxed), config.iterations);
            assert_eq!(finished.load(Ordering::Relaxed), config.iterations);
            store.shutdown().await?;
            drop(store);
            assert!(
                released.upgrade().is_none(),
                "feature and Task captures released"
            );
            Ok(WorkResult {
                operations,
                checksum,
            })
        }

        /// Holds ordered rows with stable IDs.
        type Rows = KeyedState<usize, u64>;
        /// Keeps three separate mounted subtrees in a fixed order.
        type Groups = [Rows; 3];

        /// Supplies a noncapturing projection for one fixed group.
        fn group_read<const INDEX: usize>(groups: &Groups) -> Option<&Rows> {
            groups.get(INDEX)
        }

        /// Supplies a noncapturing mutable projection for one fixed group.
        fn group_write<const INDEX: usize>(groups: &mut Groups) -> Option<&mut Rows> {
            groups.get_mut(INDEX)
        }

        /// Changes only the middle subtree during churn.
        enum Action {
            /// Removes its first row and contribution.
            Remove,
            /// Restores that stable ID with its original value.
            Insert,
        }
        /// Retains the root owner until actor teardown.
        struct Root {
            /// Shares a weakly observed owner with child features and recipes.
            _owner: Owner<()>,
        }

        impl Feature for Root {
            type State = Groups;
            type Action = Action;
            fn update(
                &mut self,
                groups: &mut Groups,
                action: &Action,
                _: &mut FeatureContext<'_, Groups, Action>,
            ) {
                match action {
                    Action::Remove => {
                        assert_eq!(groups[1].remove(&0), Some(2));
                    }
                    Action::Insert => {
                        groups[1].insert(0, 2);
                    }
                }
            }
        }

        /// Owns one fixed subtree without producing unrelated work.
        struct Group;
        impl Feature for Group {
            type State = Rows;
            type Action = ();
            fn update(&mut self, _: &mut Rows, _: &(), _: &mut FeatureContext<'_, Rows, ()>) {}
        }

        /// Retains each installed row owner until its physical release.
        struct Child {
            /// Contributes to the complete-tree release witness.
            _owner: Owner<()>,
        }
        impl Feature for Child {
            type State = u64;
            type Action = ();
            fn update(&mut self, _: &mut u64, _: &(), _: &mut FeatureContext<'_, u64, ()>) {}
        }

        /// Builds the same three-group topology for root and middle-subtree reads.
        #[expect(
            clippy::too_many_lines,
            reason = "one ordered fixture keeps topology, reads and teardown together"
        )]
        pub(super) async fn folds(
            config: &Config,
            case: Case,
            runtime: domain::Runtime,
        ) -> Result<WorkResult> {
            let Kind::Fold {
                subtree,
                sparse,
                empty,
                churn,
            } = case.kind
            else {
                unreachable!()
            };
            let width = if empty { 0 } else { config.size };
            let stride = if sparse { 16 } else { 1 };
            let contributors = if width == 0 {
                0
            } else {
                (width - 1) / stride + 1
            };
            let owner = Owner::new(());
            let released = Owner::downgrade(&owner);
            let calls = Arc::new(AtomicU64::new(0));
            let fold = Fold::<Groups, Sum>::new();
            let groups = std::array::from_fn(|group| {
                (0..width)
                    .map(|id| (id, u64::try_from(group + 1).expect("three groups")))
                    .collect()
            });
            let mut builder = Store::builder(
                groups,
                Root {
                    _owner: Owner::clone(&owner),
                },
                runtime,
            );
            let mut selected_site = None;
            for (group, (read, write)) in [
                (
                    group_read::<0> as fn(&Groups) -> Option<&Rows>,
                    group_write::<0> as fn(&mut Groups) -> Option<&mut Rows>,
                ),
                (group_read::<1>, group_write::<1>),
                (group_read::<2>, group_write::<2>),
            ]
            .into_iter()
            .enumerate()
            {
                let declared = fold.clone();
                let selected = Arc::clone(&calls);
                let child_owner = Owner::clone(&owner);
                let mount = OptionalMount::new(
                    read,
                    write,
                    move |_: &Groups| {
                        Some(rca_core::MountIdentity {
                            key: group,
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Group,
                );
                let (next, site) = builder.mount_with_children(mount, move |_, children| {
                    let declared = declared.clone();
                    let selected = Arc::clone(&selected);
                    let child_owner = Owner::clone(&child_owner);
                    children.keyed_with_children(
                        |rows| rows,
                        |rows| rows,
                        move |id, _| {
                            let contributes = (*id).is_multiple_of(stride);
                            let declared = declared.clone();
                            let selected = Arc::clone(&selected);
                            let child = EntryMount::new(Child {
                                _owner: Owner::clone(&child_owner),
                            });
                            let contribute = move |_: &u64,
                                                   children: &mut MountChildren<
                                '_,
                                Groups,
                                Action,
                                u64,
                                (),
                            >| {
                                if contributes {
                                    let selected = Arc::clone(&selected);
                                    children.contribute(&declared, move |value| {
                                        selected.fetch_add(1, Ordering::Relaxed);
                                        SumValue(Cell::new(*value))
                                    });
                                }
                            };
                            (child, contribute)
                        },
                    );
                });
                builder = next;
                if group == 1 {
                    selected_site = Some(site);
                }
            }
            drop(owner);
            let store = builder.build()?;
            store.ready().await?;
            let site = selected_site.expect("middle group declared");
            let view = store
                .acquire_fold(&fold, &site)
                .await?
                .expect("middle group mounted");
            let contributors = u64::try_from(contributors).expect("bounded contribution count");
            let full_sum = contributors * if subtree { 2 } else { 6 };
            let mut result = WorkResult::default();
            for _ in 0..config.iterations {
                if churn {
                    finish_success(store.send(Action::Remove).await?).await;
                    result.operations += 1;
                    let value = if subtree {
                        view.read().await?
                    } else {
                        store.fold(&fold).await?
                    };
                    assert_eq!(value.0.get(), full_sum - 2, "removed middle contribution");
                    result.checksum += value.0.get();
                    result.operations += 1;
                    finish_success(store.send(Action::Insert).await?).await;
                    result.operations += 1;
                }
                let value = if subtree {
                    view.read().await?
                } else {
                    store.fold(&fold).await?
                };
                assert_eq!(value.0.get(), full_sum, "selected fold sum");
                result.checksum += value.0.get();
                result.operations += 1;
            }
            let full_calls = contributors * if subtree { 1 } else { 3 };
            let per_iteration = if churn {
                2 * full_calls - 1
            } else {
                full_calls
            };
            assert_eq!(
                calls.load(Ordering::Relaxed),
                per_iteration * config.iterations,
                "every selected contribution exactly once per read"
            );
            store.shutdown().await?;
            drop(store);
            assert!(
                released.upgrade().is_none(),
                "root, child features and recipes released"
            );
            assert_eq!(
                view.read().await.err().expect("closed subtree").reason(),
                rca_core::TaskAccessReason::StoreClosed
            );
            assert_eq!(
                calls.load(Ordering::Relaxed),
                per_iteration * config.iterations
            );
            drop((view, site, fold));
            Ok(result)
        }
    };
}

/// Instantiates the real Shared carrier with atomic owner references.
mod shared {
    use rca_core as domain;
    use std::sync::Arc as Owner;
    carrier!(false);
}

/// Instantiates the real Local carrier with non-Send owner references.
mod local {
    use rca_core::local as domain;
    use std::rc::Rc as Owner;
    carrier!(true);
}

/// Uses the owned Tokio host and disposes Local work on its owner thread.
async fn execute(config: &Config, runtime: Runtime) -> Result<WorkResult> {
    let case = config.case;
    if case.local {
        let local = tokio::task::LocalSet::new();
        let result = local
            .run_until(async {
                let runtime = rca_core::local::Runtime::new(LocalTokio(runtime));
                match case.kind {
                    Kind::Task { named } => {
                        local::tasks(config, if named { config.size } else { 0 }, runtime).await
                    }
                    Kind::Fold { .. } => local::folds(config, case, runtime).await,
                }
            })
            .await;
        drop(local);
        result
    } else {
        match case.kind {
            Kind::Task { named } => {
                shared::tasks(config, if named { config.size } else { 0 }, runtime).await
            }
            Kind::Fold { .. } => shared::folds(config, case, runtime).await,
        }
    }
}

/// Adapts Local tasks to the owned Tokio `LocalSet` without a test scheduler.
#[derive(Debug)]
struct LocalTokio(Runtime);

impl rca_core::local::RuntimeDriver for LocalTokio {
    fn spawn(&self, task: rca_core::local::RuntimeFuture) {
        drop(tokio::task::spawn_local(task));
    }

    fn now(&self) -> rca_core::MonotonicInstant {
        self.0.now()
    }

    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> rca_core::local::RuntimeFuture {
        rca_core::ClockDriver::sleep_until(&self.0, deadline)
    }

    fn yield_now(&self) -> rca_core::local::RuntimeFuture {
        Box::pin(tokio::task::yield_now())
    }
}

/// Measures construction, checked work and full owned-host disposal together.
///
/// # Errors
/// Returns host construction, Store rejection or fold access failures.
///
/// # Panics
/// Panics if work counts, fold results, affinity or owner release differ from the contract.
#[inline(never)]
pub(crate) fn profile_task_fold_work(config: &Config) -> Result<WorkResult> {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let result =
        host.block_on(async { execute(config, rca_tokio::TokioRuntime::current()?).await });
    drop(host);
    let result = result?;
    assert_eq!(
        u128::from(result.operations),
        config.case.operations_per_iteration() * u128::from(config.iterations)
    );
    assert_eq!(
        u128::from(result.checksum),
        config
            .case
            .checksum_per_iteration(u64::try_from(config.size).expect("bounded size"))
            * u128::from(config.iterations)
    );
    Ok(result)
}
