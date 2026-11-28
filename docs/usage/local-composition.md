# Compose reducers that borrow local data

A child reducer can borrow local data for one call without creating a Store.
For example, a parent can pass part of its state to a child that updates a counter.
The child returns before the parent uses the counter again.
That is different from keeping the counter in a task while the task waits for a reply.
For the basics of parent and child reducers, see [compose child features](composition.md).

## Two different meanings of local

| What does your application need? | Use | What happens to the values? |
| --- | --- | --- |
| Borrow data for one reducer call | Ordinary reducer composition | The call can borrow state. A returned effect cannot keep that borrow. |
| Keep values on one thread across an asynchronous wait | `rca::local` with the `local` Cargo feature | The owner thread must run and destroy the work. Values that the Store retains must satisfy `'static`. |
| Run thread-safe work on a single Tokio thread | The ordinary `Store` and `TokioRuntime` | The shared Store still requires values it can send between threads. The number of executor threads does not change this requirement. |

Rust calls a type `Send` if it can move safely between threads.
`Sync` means that different threads can safely share a reference to the value.
A `'static` bound does not mean that a value lives forever.
It means that the value cannot contain a reference that expires earlier.
An owned `Rc` (a single-thread reference-counted pointer) can satisfy `'static` without satisfying `Send`.
A reference to a temporary local variable cannot satisfy `'static`.

If work must stay on one thread after the call, see [owner-thread features](structural-features.md#keep-thread-affine-work-on-its-owner).
Even in a Local Store, dependency keys and values must satisfy `Send + Sync + 'static`.

## What remains retained

With ordinary `Scope`, the child works on part of the parent's state during the call.
This rule also applies to `SharedScope`, `ScopeIdentified`, `IfSome`, `IfSomeIdentified`, `IfCase`, and `IfCaseIdentified`.
Those wrappers do not start another task or choose a runtime.
The table distinguishes values they borrow during the call from values that follow-up work can own.

| Value | Requirement for these wrappers |
| --- | --- |
| Parent and child state | No extra `Send` or `'static` bound |
| Parent and child reducers | Implement the required `Reducer` or `SharedReducer` trait. The wrapper adds no requirement to send them between threads. |
| Callbacks that select or split state, extract an action, or select an ID | They can borrow local data or own non-`Send` values. Callers can use their returned references only for the lifetime of the input borrow. |
| Parent and child action types | `'static`, but not necessarily `Send` |
| Callback that turns a child action into a parent action | `Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static` |
| The selected ID for an identified scope | `Eq + Hash + Clone + Send + Sync + 'static` |

Two callbacks handle actions in opposite directions. The *extractor* borrows a child action from the current parent action.
The *embedder* wraps an action from a child effect in a new parent action.
That effect can run later, so it owns a clone of the embedder.
An embedder can be a function without captures or a closure that owns a thread-safe `Arc` handle.

The ID callback and the ID itself also have different requirements.
For example, a callback can read a borrowed counter and return a reference to an owned `u64` field.
RCA can keep a copy of that ID after the call. An ID that contains an `Rc` or borrows a temporary string does not qualify.

## Actions and effects

An action can own a value that cannot be copied or sent to another thread.
For example, such an action can contain an `Rc`. That does not make every effect constructor accept it.
The wrapper must support a child effect that returns an action later.
For this reason, its action types still require `'static`, even when one call returns `Effect::none()`.
A leaf reducer can accept an action with a shorter borrow without satisfying that requirement.

An owned effect cannot retain a reference to the state from the call.
A callback that splits state must return two mutable references to separate parts of that state.
Both references last only as long as the input borrow.

## Store admission stays separate

The ordinary `Store::new` and Store builder require `Send + 'static` state, actions, and features.
For example, a reducer that captures an `Rc` works in a direct call but cannot move to the shared Store.
A reducer that borrows an `AtomicUsize` can satisfy `Send` without satisfying `'static`.

Rust also checks the reducer type separately from its inputs.
For example, a capture-free wrapper can satisfy `Send + Sync` even if its child state is not `Send`.
The wrapper does not own the state. A wrapper that stores an `Rc` callback does not satisfy `Send`.
Neither case changes what the Store requires of its state.

## Executable compiler examples

The [shared scope example](../../crates/rca-core/tests/ui/rc04_local_shared_scope.rs) shows both borrowed callbacks and an owned `Rc`.
The [identified scope example](../../crates/rca-core/tests/ui/rc04_local_identified_lifecycle.rs) separates the ID callback from the ID it selects.
Both examples call `Reducer::reduce` in a function that receives an RCA `ReduceContext`.
The [compiler tests](../../crates/rca-core/tests/compile.rs) check which types Rust accepts. They do not run the reducers.
The [composition tests](../../crates/rca-core/tests/composition.rs) exercise state changes and effect routing.

The table does not describe every child wrapper. Keyed, presentation, ephemeral, and stack reducers have their own requirements.
See the [composition guide](composition.md#choose-the-smallest-composition-model) for the choice between them.
