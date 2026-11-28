//! Checks sibling links when a parent mixes keyed collections with other children.
use super::*;
use crate::effect::lineage::ControlWake;
use crate::store::carriers::Shared;
use crate::store::command::Command;
use crate::store::ingress::CommandIngress;
use crate::store::mount::{EntryMount, MountIdentity, OptionalMount};
use crate::{Feature, FeatureContext, KeyedState};
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Supplies an optional descendant beneath each retained keyed child.
struct Row {
    detail: Option<usize>,
}
/// Keeps two independent keyed declarations and an optional child under one parent.
struct Group {
    first: KeyedState<u8, Row>,
    second: KeyedState<u8, Row>,
    optional: Option<Row>,
}
/// Holds the group while declaration reconciliation owns structural changes.
struct Parent;
impl Feature for Parent {
    type State = Group;
    type Action = bool;
    fn update(&mut self, _: &mut Group, _: &bool, _: &mut FeatureContext<'_, Group, bool>) {}
}
/// Retains mutable feature-local history so relinking cannot reconstruct the feature.
struct Entry(Cell<usize>);
impl Feature for Entry {
    type State = Row;
    type Action = u8;
    fn update(&mut self, _: &mut Row, _: &u8, _: &mut FeatureContext<'_, Row, u8>) {
        self.0.set(self.0.get() + 1);
    }
}
/// Gives each keyed child an independently retired descendant.
struct Detail;
impl Feature for Detail {
    type State = usize;
    type Action = char;
    fn update(&mut self, _: &mut usize, _: &char, _: &mut FeatureContext<'_, usize, char>) {}
}
/// Creates one row with its descendant present before the first reconciliation.
fn row() -> Row {
    Row { detail: Some(7) }
}

#[test]
fn exhausted_tokens_still_rebuild_an_unchanged_roster_with_a_retired_child() {
    let makes = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&makes);
    let (owner, _) = crate::store::mount::children::attach::<Option<Group>, (), _, _>(
        OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |group: &Option<Group>| {
                group.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Parent,
        ),
        move |_, children| {
            let calls = Arc::clone(&calls);
            children.keyed(
                |group| &group.first,
                |group| &mut group.first,
                move |_, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    EntryMount::new(Entry(Cell::new(0)))
                },
            );
        },
    );
    let mut state = Some(Group {
        first: [(1, row())].into_iter().collect(),
        second: KeyedState::new(),
        optional: None,
    });
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::MIN);
    let ingress = Arc::new(CommandIngress::<Command<Option<Group>, ()>>::new(
        sender,
        ControlWake::new(),
    ));
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    owner.bind(&ingress, &lifecycle);
    let mut mounts = vec![owner];
    let mut nodes = Vec::new();
    let mut reconcile = |state: &Option<Group>, scopes: &mut ScopeRegistry| {
        crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
            &mut mounts,
            true,
            state,
            scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
    };

    reconcile(&state, &mut scopes);
    reconcile(&state, &mut scopes);
    let [parent]: [_; 1] = ring(&scopes, scopes.root()).try_into().unwrap();
    let [first]: [_; 1] = ring(&scopes, parent).try_into().unwrap();
    let first_authority = scopes.authority(first);
    assert!(scopes.accepts(&first_authority));

    state.as_mut().unwrap().first.remove(&1);
    reconcile(&state, &mut scopes);
    assert_eq!(ring(&scopes, parent), [] as [ScopeToken; 0]);
    assert!(!scopes.accepts(&first_authority));
    let unrelated = scopes.mount(scopes.root(), ScopeNodeId::new(), ScopeIdentity::unit());
    assert_eq!(scopes.leave(unrelated), [unrelated]);
    reconcile(&state, &mut scopes);
    assert_eq!(ring(&scopes, parent), [] as [ScopeToken; 0]);
    assert_eq!(makes.load(Ordering::Relaxed), 1);

    state.as_mut().unwrap().first.insert(1, row());
    reconcile(&state, &mut scopes);
    let [child]: [_; 1] = ring(&scopes, parent).try_into().unwrap();
    let old = scopes.authority(child);
    assert_ne!(child, first);
    assert!(scopes.accepts(&old));
    assert!(!scopes.accepts(&first_authority));

    scopes.next_token = u64::MAX;
    reconcile(&state, &mut scopes);
    reconcile(&state, &mut scopes);
    assert_eq!(makes.load(Ordering::Relaxed), 2);
    assert_eq!(scopes.leave(child), [child]);
    assert!(!scopes.accepts(&old));

    // A wrongly reused proof would skip the factory; the stale declaration would
    // then hit the same exhaustion panic while trying to install its own token.
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reconcile(&state, &mut scopes);
    }))
    .expect_err("installing the replacement needs a fresh scope token");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("scope token ID space exhausted"));
    assert_eq!(makes.load(Ordering::Relaxed), 3);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one parent projection schedule compares both collections and retained legacy slots"
)]
fn two_collections_optional_and_legacy_slots_follow_one_actual_parent_projection() {
    let makes = Arc::new(AtomicUsize::new(0));
    let calls = std::sync::Arc::clone(&makes);
    let (owner, _) = crate::store::mount::children::attach::<Option<Group>, (), _, _>(
        OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<Group>| {
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Parent,
        ),
        move |_, children| {
            let calls = std::sync::Arc::clone(&calls);
            children.keyed_with_children(
                |group| &group.first,
                |group| &mut group.first,
                move |_, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    (EntryMount::new(Entry(Cell::new(0))), |_, children| {
                        children.optional(OptionalMount::new(
                            |row: &Row| row.detail.as_ref(),
                            |row: &mut Row| row.detail.as_mut(),
                            |row: &Row| {
                                row.detail.map(|_| MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            |_| Detail,
                        ));
                    })
                },
            );
            children.optional(OptionalMount::new(
                |group: &Group| group.optional.as_ref(),
                |group: &mut Group| group.optional.as_mut(),
                |group: &Group| {
                    group.optional.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Entry(Cell::new(0)),
            ));
            children.keyed(
                |group| &group.second,
                |group| &mut group.second,
                |_, _| EntryMount::new(Entry(Cell::new(0))),
            );
        },
    );
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<Option<Group>, ()>>::new(
        sender,
        ControlWake::new(),
    ));
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    owner.bind(&ingress, &lifecycle);
    let mut mounts = vec![owner];
    let mut nodes = Vec::new();
    let mut state = Some(Group {
        first: [(1, row()), (2, row())].into_iter().collect(),
        second: [(3, row()), (4, row())].into_iter().collect(),
        optional: Some(row()),
    });
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    let parent = mounts[0].installed_scope().unwrap();
    let [a1, a2, optional, b3, b4]: [_; 5] = ring(&scopes, parent).try_into().unwrap();
    let legacy: Vec<_> = (0..3)
        .map(|_| scopes.mount(parent, ScopeNodeId::new(), ScopeIdentity::unit()))
        .collect();
    let legacy_descendants: Vec<_> = legacy
        .iter()
        .map(|token| scopes.mount(*token, ScopeNodeId::new(), ScopeIdentity::unit()))
        .collect();
    let initial = [legacy[0], a1, legacy[1], a2, optional, b3, legacy[2], b4];
    // Fixture setup deliberately interleaves the independent legacy scopes.
    scopes.relink_native_children(parent, &initial);
    let retained = [a1, a2, optional, b4];
    let identities: Vec<_> = retained
        .iter()
        .map(|token| {
            scopes.refresh_configuration(*token);
            (
                *token,
                scopes.records[token].mount.node.clone(),
                scopes.authority(*token),
            )
        })
        .collect();
    let registrations: Vec<_> = mounts
        .iter()
        .filter_map(|owner| {
            owner
                .installed_scope()
                .map(|token| (token, owner.registration_identity()))
        })
        .collect();
    let group = state.as_mut().unwrap();
    group.first.reverse();
    group.first.insert(5, row());
    group.second.reverse();
    group.second.remove(&3);
    let mut transitions = ScopeTransitionBatch::default();
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut transitions,
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    let native: Vec<_> = mounts
        .iter()
        .filter_map(|owner| owner.installed_scope())
        .filter(|token| scopes.parent(*token) == Some(parent))
        .collect();
    assert_eq!(&native[..2], &[a2, a1]);
    assert_eq!(&native[3..], &[optional, b4]);
    let inserted = native[2];
    assert_eq!(
        ring(&scopes, parent),
        [
            legacy[0], a2, legacy[1], a1, inserted, legacy[2], optional, b4
        ]
    );
    assert_eq!(
        makes.load(Ordering::Relaxed),
        3,
        "one newly constructed Feature, retained caches not rebuilt"
    );
    assert!(transitions.contains(b3));
    assert_eq!(transitions.membership.as_ref().unwrap().len(), 1);
    #[cfg(feature = "diagnostics")]
    assert!(transitions.iter().eq([b3]));
    for (token, node, authority) in identities {
        assert_eq!(scopes.records[&token].mount.node, node);
        assert!(scopes.accepts(&authority));
        let registration = mounts
            .iter()
            .find(|owner| owner.installed_scope() == Some(token))
            .unwrap()
            .registration_identity();
        assert_eq!(
            registration,
            registrations
                .iter()
                .find(|(old, _)| *old == token)
                .unwrap()
                .1
        );
    }
    // Unchanged pass must preserve the complete mixed ring.
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    assert_eq!(
        ring(&scopes, parent),
        [
            legacy[0], a2, legacy[1], a1, inserted, legacy[2], optional, b4
        ]
    );
    let a1_children = ring(&scopes, a1);
    let old_authority = scopes.authority(a1);
    let before = makes.load(Ordering::Relaxed);
    assert_eq!(
        scopes.leave(a1),
        a1_children.into_iter().chain([a1]).collect::<Vec<_>>()
    );
    // A preceding retirement can invalidate a direct entry without any State edit.
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    let remounted = mounts
        .iter()
        .filter_map(|owner| owner.installed_scope())
        .filter(|token| scopes.parent(*token) == Some(parent))
        .nth(1)
        .unwrap();
    assert_eq!(makes.load(Ordering::Relaxed), before + 1);
    assert_ne!(remounted, a1);
    assert!(scopes.accepts(&scopes.authority(remounted)));
    assert!(!scopes.accepts(&old_authority));
    assert_eq!(ring(&scopes, remounted).len(), 1);

    let siblings = ring(&scopes, parent);
    let mut expected = Vec::new();
    for sibling in siblings {
        expected.extend(ring(&scopes, sibling));
        expected.push(sibling);
    }
    expected.push(parent);
    let retired = scopes.leave(parent);
    assert_eq!(retired, expected);
    let unique: std::collections::HashSet<_> = retired.iter().copied().collect();
    assert_eq!(unique.len(), retired.len());
    assert!(
        retired
            .iter()
            .chain(legacy_descendants.iter())
            .all(|token| !scopes.is_current(*token))
    );
    assert_eq!(ring(&scopes, scopes.root()), [] as [ScopeToken; 0]);
}
