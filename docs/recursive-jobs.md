# Finite recursive jobs and flat stacks

Choose the structure by what happens to the other jobs when one job ends:

- If removing a job must also remove every later job on the same path, use a flat `StackState<Job>`.
- If siblings can end independently, use `KeyedState` with stable business IDs.
- If each job owns jobs of its own, use a tree. Decide how removal works at each level.

These choices describe ownership, not presentation. A `Vec` alone does not
tell RCA when a child ends or which delayed outputs to reject.
The `finite_jobs` module in the [stack scope tests](../rca-test/tests/stack_scopes.rs)
compares delayed leaf updates in a flat stack and a tree of stacks. Both produce
`10 -> 11` and `30 -> 31`. The tree's ancestor values do not change.

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

The test first sends an action through a grandchild's `EffectSender`.
It then removes the ancestor with `StackAction::PopFrom`. The descendant's
timer and pending effect end. The captured sender rejects further output, and
the ancestor's `ScopedStore` ends. An earlier sibling stays active: its timer
finishes and updates only that sibling. The stack also removes any later siblings.

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
rustup run 1.100.0 cargo test --locked -p rca-test --test stack_scopes
```

The bounds belong to this recipe, not a global maximum for RCA applications.
Changing them requires new decoder, routing, retirement and destruction tests.
For installed child instances and private command routes, see
[structural features](structural-features.md). For the wider evidence boundary,
see [qualification](qualification.md).
