# Model jobs that own other jobs

A job can start child jobs. Choose a representation by what must happen to those children when a job ends.

| What happens on removal? | Representation | Example |
| --- | --- | --- |
| The selected job and all later jobs must stop together. | A flat `StackState<Job>` | Discard a processing stage and the stages that depend on it. |
| Jobs at the same level can stop independently. | `KeyedState` with stable IDs | Cancel one upload without stopping the others. |
| Each job owns children of its own. | A tree with an explicit removal rule at each level | Stop a pipeline branch and the work below it. |

These choices concern ownership, not screen layout. A `Vec` by itself does not tell RCA which late messages to reject.
For the basic concepts and other ways to compose features, see [manual composition](composition.md).

## Finite boundary

The [working example](../../crates/rca-test/tests/stack_scopes.rs) compares a flat stack with a small tree.
In the tree, each job has a number and a stack of children: `Job { value: u8, children: StackState<Job> }`.
A **leaf** is a job with no children. Two `stack` adapters connect the root reducer to its grandchildren.
The reducer updates a leaf after a delay. The numbers ten and thirty each increase by one in both representations.
Updating a grandchild leaves the numbers on its parent and grandparent unchanged.

The example reads the entire tree before it creates the Store.
Each input pair gives a job's number and child count. The pairs for its children follow immediately.
For example, `[1, 1, 2, 0]` describes job 1 with one child, job 2 with no children.

| Resource | Limit in this example |
| --- | --- |
| Input | 14 bytes for seven pairs |
| Tree | Seven jobs, two children per job, three levels including the root |
| Recursive read | Three active calls at most. A job at level three cannot have children. |
| Action from root to leaf | Two nested `Box` values at most. Each holds the next stack action. |
| Destruction | Ordinary Rust destruction of up to seven jobs across three levels |

Before it allocates any jobs, the decoder rejects an input longer than 14 bytes.
Before it constructs each job, it checks the depth and child count. It rejects incomplete pairs and extra bytes after the tree.
New jobs receive fresh `StackId` values. Loading the same bytes does not restore permission to send messages to an old job.

The Store can change job values or remove the end of a stack. It cannot grow this particular tree.
Its push input is `Infallible`, a type with no possible value.
These limits belong to this example. They are not a memory budget or a limit on all RCA applications.

## Lifetime and ownership

The [removal test](../../crates/rca-test/tests/stack_scopes.rs) starts a timer on a grandchild and another on an earlier branch.
The test sends a value to the grandchild, then removes the grandchild's parent with `StackAction::PopFrom`.
The operation removes the parent, its descendants, and all later siblings on that stack.
The grandchild's timer and pending effect end. The earlier branch stays active. Its timer finishes and changes only its own value.

The removed branch no longer accepts output. An `EffectSender::send` on the cancelled effect returns an error, not the action.
A `ScopedStore` is a handle for one route to a child. If that route has ended, its `try_send` returns the original rejected action.
The test checks that the owned argument retains its value, target ID, and `Box` address without a clone.

## Try the example and understand its limits

The example tests malformed input, the largest permitted tree, delayed updates, and work left after removal.
Each representation runs two delayed effects. The removal case has two timers, a pending effect, and a retained sender.
These tests do not measure allocation size or speed. They do not establish that arbitrary-depth routing or destruction is safe.
If your application accepts deeper trees, give decoding, routing, removal, and destruction their own limits and tests.

To run the example, start in the RCA repository root.
On macOS, pass the Cargo arguments to the [build memory guard](../../CONTRIBUTING.md#build-memory-safety).
On an isolated runner with a memory limit, run:

```sh
rustup run 1.100.0 cargo test --locked -p rca-test --test stack_scopes
```

For children with an independent lifetime or private input routes, see [installed features](structural-features.md).
