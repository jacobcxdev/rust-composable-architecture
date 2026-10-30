# Local synchronous composition

Synchronous composition accepts local state and callbacks without requiring a local asynchronous runtime.

## Two different meanings of local

Borrowed synchronous composition and owner-thread execution solve different needs.

| Need | Surface | Ownership boundary |
| --- | --- | --- |
| Compose reducers over borrowed or non-`Send` values during a synchronous call | The ordinary composition functions described here | Borrowing ends when the call returns. Returned effects retain their own bounds. |
| Keep non-`Send` state, actions or future captures across asynchronous suspension | `rca::local` with the `local` feature | The owner thread must poll and destroy the work. Stored values still need `'static`. |
| Run ordinary `Send` state and work on a current-thread Tokio runtime | The shared `Store` and `TokioRuntime` | Choosing one executor thread does not remove the shared API's transport bounds. |

`'static` does not mean that a value lives forever. It means that the stored
value cannot depend on a shorter-lived borrowed reference. An owned `Rc` can
satisfy this bound. A reference into a caller's temporary stack frame cannot.

Local execution does not remove every bound from every API. Dependency keys and
values still use `DependencyValues` and its `Send + Sync + 'static` contract.
Synchronous action embedding and identity metadata have the separate requirements
listed below. Use their actual signatures, not the word "local", to decide eligibility.

`SharedScope`, `ScopeIdentified`, `IfSome`, `IfSomeIdentified`, `IfCase` and
`IfCaseIdentified` can call their children with borrowed or non-`Send` state.
Their projection, split, extraction and identity callbacks may also borrow
local data or own non-`Send` captures. The parent and child reducers are called
synchronously; their concrete stored fields determine their own lifetimes and
auto traits.

This is the same separation already used by ordinary `Scope`. These synchronous
adapters do not create an actor or choose an asynchronous runtime.
RCA provides owner-local asynchronous execution through `rca::local::Store`. See the
[structural Feature guide](structural-features.md#keep-thread-affine-work-on-its-owner)
for that route. The bounds in this guide describe synchronous composition only.

## What remains retained

The requirements differ because only some values enter owned follow-up work:

| Value | Requirement when used as a composed `Reducer` |
| --- | --- |
| Parent and child state | No added `Send` or `'static` bound |
| Parent and child reducers | Matching `Reducer` or `SharedReducer`; no added transport bound |
| State projections, shared split, action extraction | Existing higher-ranked callable shape; no added `Send` or `'static` bound |
| Identity callback | Higher-ranked borrow of the selected ID; no added `Send` or `'static` bound |
| Parent and child action types | `'static`, but not necessarily `Send` |
| Action embedder | `Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static` |
| Identified scope's ID value | `Eq + Hash + Clone + Send + Sync + 'static` |

The embedder is cloned into owned effect mapping. The extractor is only called
while borrowing the current input action. Consequently, a local extractor does
not require a local embedder: a capture-free function or an owned lawful
`Arc`-capturing mapper can lift effects from locally selected state.

The ID callback is similarly distinct from the ID value. A callback may read
a borrowed local counter and return `&child.id` where `id` is an owned `u64`.
An ID value containing an `Rc` or borrowing a local string does not meet the
registry's retained metadata requirements.

## Actions and effects

Owned move-only actions containing an `Rc` are valid inputs to these synchronous
composed reducers. This does not mean that every effect constructor accepts
those actions. Effect constructors and asynchronous captures keep their own
requirements.

Parent and child action types must still be `'static` because the total
composition implementation maps the concrete `Effect` representation. A child
returning `Effect::none()` in one call does not remove that requirement.
Arbitrary-lifetime borrowed actions supported by a local leaf reducer therefore
cannot necessarily be lifted through a scope.

No reference into selected state may escape into an owned effect. Shared split
projections must still return disjoint mutable references. Their higher-ranked
signatures tie the returned references to the input borrow; transport-bound
relaxation does not weaken borrowing or aliasing rules.

## Store admission stays separate

Actor-backed `Store::new` and the Store builder still require state, actions
and the reducer to be `Send + 'static`. A composed reducer with an owned `Rc`
projection can be used synchronously but cannot enter that Store. A reducer
borrowing an `AtomicUsize` can satisfy `Send` yet still fail Store's `'static`
requirement.

Auto traits remain structural. A capture-free adapter over a non-`Send` domain
can itself be `Send + Sync`, because the domain appears only in function-pointer
markers. An adapter actually storing an `Rc` callback is not `Send`. Neither
case overrides the independent Store payload requirements.

## Executable compiler examples

The ordinary-import compiler fixtures contain real `Reducer::reduce` calls
inside functions that receive a framework-provided `ReduceContext`:

- [Shared scope local domains](../rca-core/tests/ui/rc04_local_shared_scope.rs)
- [Identified and lifecycle local domains](../rca-core/tests/ui/rc04_local_identified_lifecycle.rs)
- [Retained and hosted compiler controls](../rca-core/tests/compile.rs)

Those call sites prove type eligibility; referring to a fixture function from
`main` does not claim runtime execution. Existing composition and lifecycle
runtime tests separately exercise child-first/base-once ordering, reconciliation
and effect routing. `ForEach`, `ForEachShared`, presentation, ephemeral and stack
adapters are outside this bound-cleanup scope and retain their independent
requirements.
