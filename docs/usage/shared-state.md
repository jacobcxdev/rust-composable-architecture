# Share a value between owners

Two owners can use `rca::Shared<T>` to read or change one value in memory.
For example, two services can share a preference without passing copies to each other.
This type needs no extra crate or Cargo feature.

If one feature makes decisions from the value, prefer ordinary Store state.
The Store processes one action at a time and publishes changes to its observers.
A write through a shared handle neither notifies those observers nor saves the value to storage.
Putting the handle in state does not make its writes part of a Store update.
See [where a value belongs](authoring.md#where-does-this-value-belong).

## Does a shared reader follow rebinding?

Rebinding means changing which value a handle refers to. Copies made with `clone()` follow the writable handle to the new value.
A reader gives read access to the original value. A projection gives access to part of that value.
Neither follows the rebind. Both still see later edits to the original value.

In this example, the writer switches to `Some(10)`. The projection then changes the original number from 1 to 2.

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

The [sharing example](../../crates/rca-core/src/sharing.rs) and
[tests for cloned handles](../../crates/rca-core/tests/shared_value.rs) exercise these two reads.

A reader cannot rebind the handle or borrow the value mutably.
Some Rust types, such as `Cell`, permit changes through a shared reference. The reader does not prevent those changes.
To keep the value at one instant, capture an independent copy instead of cloning the handle.
The [namespace tests](../../crates/rca-test/tests/shared_namespaces.rs) do this with `TestStore` and `with_capture`.

## Change a value and notify another owner

Readers and writers access the value through short callbacks that return before access ends.
If another caller holds the value, access returns `Busy` instead of waiting.
If a callback panics while it holds the value, later access reports `Poisoned`.
If the selected part is missing, a projection reports `Absent`. It does not recreate that optional value, enum case, or keyed child.

An update changes the value in place. If the update returns `Err`, earlier writes still stand.
After it releases the lock, RCA calls your second callback to announce the write.
RCA calls it even for an unchanged value or an update that returned `Err`.
If the update panics, RCA skips the announcement. If access fails or the projection is absent, neither callback runs.

Another thread can change the value before the announcement starts. The announcements need not run in write order.
If you need the exact result of your write, return an owned copy from the update.

Keeping the shared data does not keep a child feature installed in its parent Store.
The [scope tests](../../crates/rca-test/tests/shared_value_scopes.rs) show a reader that still works after removal.
The Store rejects later replies from the removed child.

## When is a shared-key cache appropriate?

If two callers need to open the same resource, give them clones of one `SharedKeyCache`.
For example, both can open a preference through a service that loads its value and reports changes.
That service is the provider. Create a separate cache for an independent session or test.
The cache holds weak references: existing handles, not the cache alone, keep values and subscriptions alive.

`SharedKeyIdentity` combines a resource address with the type and configuration of its codec (the converter for its data format).
The address must distinguish different backends that use the same resource name.
Equal identity means compatible access to the same resource, not merely equal
current data.

| Operation | Use when | Important boundary |
| --- | --- | --- |
| `shared` or `reader` | Empty storage permits an initial value | An existing compatible entry skips the initial value, load, and subscription. |
| `require` or `require_reader` | Stored data must exist | The provider reads storage even for an existing entry. The caller can still receive the cached value. |
| `reload` or `reload_reader` | Refresh the same resource | If storage has no value, the handle keeps its current value. |
| `load` or `load_reader` | Switch to another key | The handle rebinds before the load. A load error does not restore its old binding. |
| `reset` | Make later lookups start without the current entries | Existing handles, subscriptions, and stored data remain. Constructors already in progress continue. |

If two reloads overlap, only the one that started last can replace the value.
Even if the later reload fails or the caller cancels it, the earlier reload cannot write.
A successful update from the provider also takes precedence over earlier reloads.

Two callers can miss the cache at the same time. Both can ask the provider to load and subscribe before the cache selects one entry.
Loading and subscribing are separate calls. If updates between those calls matter, the provider must replay them or prevent that gap.

`SharedKey::save` asks the provider to save. Changing a shared value does not call it.
If the value must survive a restart, choose [a save policy](persistence.md). The [cache tests](../../crates/rca-core/tests/shared_key_cache.rs)
show how lookup, reload, and reset affect existing handles.
