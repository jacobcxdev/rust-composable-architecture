//! Checks complete sibling rings after native-only reordering.

use super::*;
mod collection_roster;

/// Reads retirement order while independently checking both directions of every link.
fn ring(scopes: &ScopeRegistry, parent: ScopeToken) -> Vec<ScopeToken> {
    let Some(list) = scopes.child_lists.get(&parent) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut cursor = list.first;
    loop {
        let record = &scopes.records[&cursor];
        assert_eq!(record.parent, parent);
        assert_eq!(
            scopes.records[&record.next_sibling].previous_sibling,
            cursor
        );
        assert_eq!(
            scopes.records[&record.previous_sibling].next_sibling,
            cursor
        );
        result.push(cursor);
        assert!(result.len() <= scopes.records.len(), "ring closes");
        if cursor == list.last {
            assert_eq!(record.next_sibling, list.first);
            break;
        }
        cursor = record.next_sibling;
    }
    assert_eq!(scopes.records[&list.first].previous_sibling, list.last);
    result
}

#[test]
fn native_subset_preserves_legacy_slots_and_stable_retirement_postorder() {
    let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
    let root = scopes.root();
    let parent = scopes.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
    let children: Vec<_> = (0..7)
        .map(|_| scopes.mount(parent, ScopeNodeId::new(), ScopeIdentity::unit()))
        .collect();
    let [l0, a, l1, b, optional, l2, new] = children.clone().try_into().unwrap();
    let descendants: Vec<_> = children
        .iter()
        .map(|child| scopes.mount(*child, ScopeNodeId::new(), ScopeIdentity::unit()))
        .collect();
    let nodes: Vec<_> = children
        .iter()
        .map(|child| {
            (
                scopes.records[child].mount.node.clone(),
                scopes.records[child].configuration,
            )
        })
        .collect();
    scopes.relink_native_children(parent, &[b, new, a, optional]);
    let expected = [l0, b, l1, new, a, l2, optional];
    assert_eq!(ring(&scopes, parent), expected);
    for (token, expected) in children.iter().zip(nodes) {
        assert_eq!(
            (
                scopes.records[token].mount.node.clone(),
                scopes.records[token].configuration
            ),
            expected
        );
    }
    scopes.relink_native_children(parent, &[b, new, a, optional]);
    assert_eq!(ring(&scopes, parent), expected);
    assert_eq!(scopes.leave(l1), [descendants[2], l1]);
    let mut expected_leave = Vec::new();
    for child in expected.into_iter().filter(|token| *token != l1) {
        expected_leave
            .push(descendants[children.iter().position(|token| *token == child).unwrap()]);
        expected_leave.push(child);
    }
    expected_leave.push(parent);
    assert_eq!(scopes.leave(parent), expected_leave);
    assert!(
        children
            .into_iter()
            .chain(descendants)
            .all(|token| !scopes.is_current(token))
    );
    assert!(ring(&scopes, root).is_empty());
}

#[test]
fn empty_singleton_and_reordered_subsets_preserve_complete_rings() {
    let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
    let root = scopes.root();
    scopes.relink_native_children(root, &[]);
    let a = scopes.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
    scopes.relink_native_children(root, &[a]);
    assert_eq!(ring(&scopes, root), [a]);
    let b = scopes.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
    let descendant = scopes.mount(a, ScopeNodeId::new(), ScopeIdentity::unit());
    assert_eq!(ring(&scopes, root), [a, b]);
    assert_eq!(ring(&scopes, a), [descendant]);
    scopes.relink_native_children(root, &[b, a]);
    assert_eq!(ring(&scopes, root), [b, a]);
    scopes.relink_native_children(a, &[descendant]);
    assert_eq!(ring(&scopes, a), [descendant]);
}
