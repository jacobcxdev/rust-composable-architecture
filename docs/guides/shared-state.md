# Share a value between owners

Use `rca::Shared<T>` when separate owners need access to the same mutable value.
It needs no extra Cargo feature or crate. If a value determines the next action
in a feature, prefer ordinary Store state. A write through a shared handle is not
a Store transaction: it neither notifies Store observers nor saves the value.
Putting the handle in state does not make that write atomic with other changes.
See [where a value belongs](authoring.md#where-does-this-value-belong).

## Does a shared reader follow rebinding?

Clones of a writable handle follow its rebinding together. A reader or projection
keeps the value the handle pointed to when you created it. It still sees later
changes to that value; it is not a snapshot.

```rust
use rca::Shared;

let value = Shared::new(Some(1));
let alias = value.clone();
let reader = value.reader().unwrap();
let child = value.project(Option::as_ref, Option::as_mut).unwrap();

value.rebind(&Shared::new(Some(10))).unwrap();
child.update(|value| *value += 1, || {}).unwrap();
assert_eq!(reader.read(|value| *value).unwrap(), Some(2));
assert_eq!(alias.read(|value| *value).unwrap(), Some(10));
```

The [sharing doctest](../../crates/rca-core/src/sharing.rs) and
[alias tests](../../crates/rca-core/tests/shared_value.rs) exercise this example.

A reader cannot rebind the handle or take a mutable borrow. If the value contains
a `Cell`, it can still change in the ways that Rust permits through `&T`.
To assert what the value was at one instant, capture an independent value instead
of cloning the handle. The [namespace tests](../../crates/rca-test/tests/shared_namespaces.rs)
do this with `TestStore` and `with_capture`.

## Change a value and notify another owner

Shared access uses short synchronous callbacks. It reports `Busy` for overlapping
access instead of waiting. A panic inside value access poisons that value.
A missing optional, case or keyed projection reports `Absent` and does not
recreate the missing payload.

An update changes the value in place. Returning `Err` does not undo an earlier
write. After unlocking, RCA calls the publication callback you supplied, even
when nothing changed or your mutation returned `Err`. A panic in the mutation
skips the callback. A failed access or missing projection never starts the mutation.
Across threads, callbacks need not run in write order. If a caller needs the
exact result of its write, return an owned snapshot from the mutation instead of
reading the handle again after unlocking.

Retaining shared data does not retain permission to send to a removed child.
The [scope tests](../../crates/rca-test/tests/shared_value_scopes.rs) show readable data
alongside rejected stale feedback.

## When is a shared-key cache appropriate?

Use `SharedKeyCache` when two callers should open the same provider-backed value.
Give them clones of one cache. Create a new cache for an independent session or
test. Its entries are weak references: existing handles, not the cache alone,
keep values and subscriptions alive.

`SharedKeyIdentity` combines a resource address, codec type and codec configuration.
The address must distinguish different backends that use the same resource name.
Equal identity means compatible access to the same resource, not merely equal
current data.

| Operation | Use when | Important boundary |
| --- | --- | --- |
| `shared` or `reader` | A seed is valid when storage is absent | A compatible live hit skips seed, load and subscription work |
| `require` or `require_reader` | Stored data must exist | Storage is loaded even on a cache hit, but the live cached value can still win |
| `reload` or `reload_reader` | Refresh the captured resource | Storage absence retains its current value |
| `load` or `load_reader` | Change to another resource key | Rebinding precedes reload, so a reload error does not restore the old binding |
| `reset` | Stop future lookups from finding current entries | Existing handles, subscriptions, storage and in-flight constructors remain |

If two reloads overlap, only the one that started last can replace the value.
Even if it fails or is cancelled, the earlier reload cannot write. A successful
producer update also takes precedence over earlier reloads.

Two callers can both miss the cache and start loading and subscribing. The cache
keeps one value, but the provider may run twice. Loading and subscribing are
separate calls. If updates between those calls matter, the provider needs a way to
replay them or avoid missing them.

`SharedKey::save` gives the provider a way to save. Changing a shared value
does not call it. Choose [an explicit save policy](persistence.md) if the value
must survive a restart. The [cache tests](../../crates/rca-core/tests/shared_key_cache.rs)
show how lookup, reload, and reset affect existing handles.
