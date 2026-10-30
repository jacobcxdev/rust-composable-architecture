# Finite recursive jobs and flat stacks

Prefer a flat `StackState<Job>` when the workload is a single ordered path. Use a
tree only when jobs actually own child jobs. The executable `finite_jobs` module
in [stack scope tests](../rca-test/tests/stack_scopes.rs) compares two delayed leaf
updates through nested stacks with the existing flat stack recipe. Both produce
`10 -> 11` and `30 -> 31`; the tree's ancestor values remain unchanged.

These choices describe ownership, not presentation. A stack can model a headless
workflow, and a tree can model nested jobs without any UI.
Use `KeyedState` for independently removable siblings with stable business IDs.
If removing an entry must also remove everything after it, use `StackState`.
A `Vec` alone does not express RCA's child lifetime or stale-output checks.

This is application composition with existing public APIs, not an arbitrary
recursive reducer facility. Its recursive data is `Job { value: u8, children:
StackState<Job> }`. Two explicit `stack` layers wrap the leaf reducer, admitting
three job levels including the root. The action owns its nested stack action in
a `Box`; each child effect is mapped back through the actual captured route.
`Infallible` push inputs prevent adding jobs through this recipe's action type.
The decoder constructs the admitted tree before installing the store.

## Finite boundary

| Resource | Admitted bound |
| --- | --- |
| Input | At most 14 bytes: preorder `(value, child count)` pairs |
| Owned tree | At most 7 jobs, 2 children per job, 3 job levels |
| Decode recursion | At most 3 node frames; a third-level node must have no children |
| Routed leaf action | At most 2 nested stack-action boxes from root to leaf |
| Owned tree drop | At most 3 job levels and 7 jobs; ordinary container destruction |
| Comparison workload | 2 delayed effects per representation, one increment per leaf |
| Retirement workload | 2 timers and 1 captured pending effect; one retained output endpoint |

Input length is checked before allocating nodes. Depth and child count are
checked before constructing each node, and incomplete or trailing input is
rejected. Validated construction is the only source of tree structure here;
the reducers only change values or remove stack suffixes. These bounds constrain
the number of domain values, container entries and route wrappers, not exact
allocator bytes or all runtime bookkeeping. There is no allocation benchmark.
Decoded IDs are fresh `StackId`s, not persisted identity or revived authority.

## Lifetime and ownership

The retirement witness captures a real grandchild `EffectSender`, successfully
uses it, and then removes its ancestor through `StackAction::PopFrom`. The
descendant timer and pending effect finish, its captured sender rejects later
output, and the ancestor's `ScopedStore` ends. A preceding sibling's timer still
finishes and updates only that sibling. Stack removal is suffix-based: a sibling
after the removed entry would also be removed. Use keyed composition for
arbitrary independently removable siblings instead of pretending a stack is an
unordered tree.

The ended scoped endpoint's `try_send` returns the original routed non-`Clone`
payload. The test checks its `Box` address, value and target ID. This is owned
admission rejection, distinct from `EffectSender::send`, whose cancellation
error does not return the action.

## Qualification scope

The tests cover finite decode rejection, maximum admitted decode/drop, delayed
routing, independent values, ancestor retirement and exact owned rejection.
They do not establish arbitrary-depth encoding, decoding, routing, dropping or
recursive reducer support. Iterative runtime scope traversal would not establish
those properties either. The example has no provider codec, relational query,
pagination, native frontend, distributed replication or performance claim.

From the repository root, run the executable tests:

```sh
rustup run 1.98.0 cargo test --locked -p rca-test --test stack_scopes
```

The bounds belong to this recipe, not a global maximum for RCA applications.
Changing them requires new decoder, routing, retirement and destruction tests.
For installed child instances and private command routes, see
[structural features](structural-features.md). For the wider evidence boundary,
see [qualification](qualification.md).
