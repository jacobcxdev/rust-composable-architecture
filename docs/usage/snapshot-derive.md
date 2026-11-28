# Keep an independent snapshot of state

A snapshot records a value as it was at one point in time.
For example, an importer can save its progress before a task changes it from 30 to 40.
The first record must still say 30. It is a value for comparison, not a backup or storage format.

## When to use a snapshot

If cloning your state produces an independent value, use an ordinary `TestStore`.
Cloning a handle to mutable data does not produce that history. Both handles still see later writes.
A file or task handle need not support cloning at all.

In these cases, use `TestStore::projected` with a function that returns just the values the test needs.
That choice of values is a **projection**.
The `Snapshot` trait gives that function a common name: `snapshot`.
The trait needs no optional Cargo feature. For a field-by-field implementation, enable `macros` on `rca` and use `#[derive(Snapshot)]`.
The import `use rca::Snapshot;` brings in both the trait and the enabled derive macro.

## Declare owned history

Here the importer owns a progress count, a list of messages, a private value, and a label.
The `Rc<()>` field stands in for a task handle. The snapshot excludes that field and hides the label in debug messages.

```rust
use rca::Snapshot;

#[derive(Snapshot)]
#[snapshot(name = ImportHistory)]
struct Importer {
    progress: usize,
    history: Vec<String>,
    private_bookkeeping: usize,
    #[snapshot(redact)]
    label: String,
    #[snapshot(omit = "Task lifetime is asserted separately")]
    task: std::rc::Rc<()>,
}
```

The macro generates a record type called `ImportHistory`. Taking a snapshot does not run the task or change the importer.
To use the record in a test, pass `Snapshot::snapshot` to `TestStore::projected`.
The [testing guide](testing.md#choose-the-state-history) explains what the test compares. The macro does not start a test or register an observer.

| What must a field contribute? | Field attribute | Result |
| --- | --- | --- |
| Its ordinary owned value | None | Calls that field's `Snapshot` implementation. Private fields count too. |
| A custom record | `with` and `value` | Calls your function to capture an owned value. |
| Its value, without showing it in debug messages | `redact` | Keeps the value for equality and replaces its debug text with `"<redacted>"`. |
| Nothing | `omit` with a non-empty reason | Leaves the field out of the record and its comparisons. |

**Redaction does not remove a secret from memory or forbid direct field access.**
If a secret must affect equality, redact it instead of omitting it.
If you omit a task handle, test its cancellation or completion separately. Omission does not excuse outstanding TestStore operations or capture assertions.
The generated documentation records each omission reason. It does not create a runtime registry.

### Capture a custom field

Use `#[snapshot(with = capture_function, value = OwnedType)]` to supply both the function and its result type.
The function borrows the field. Each capture function runs once in field declaration order.
Separate reads do not form an atomic operation. If another owner can change a shared value, decide which values must agree.

There is no automatic capture for all `Clone` types, references, shared handles, locks, or cells.
Return an owned result independent of both the original value and later edits to expected history.
A `Clone` implementation that reintroduces shared mutable state breaks that independence.
The derive rejects explicit references, raw pointers, and function pointers as `value` types. A type alias or custom function can still hide shared access.
The compiler cannot establish this independence for you.

### Use an existing record type

To capture into an existing struct, use `#[snapshot(into = ExistingHistory)]` instead of `name`.
The source must have named fields. Its destination can be a named-field struct or an alias for one, including a generic type.
Every retained field must match a visible destination field in name and type. Omitted fields do not appear in the constructor.
Rust reports missing, extra, private, or incompatible fields.

The destination defines its traits and visibility. `redact` cannot change that destination's `Debug` implementation.
Use either `name` or `into`, never both. These options also avoid a collision with an existing type name.

## What does the generated type contain?

By default, the macro generates a companion type with one type parameter per retained field.
It stores the captured values, not the source lifetimes, bounds, or omitted marker fields.
To name the result without listing its type parameters, use `<Importer as Snapshot>::Value`.

- The companion implements `Clone`, `PartialEq`, and `Eq` if its stored values implement the corresponding trait.
- Its `Debug` output requires `Debug` only for unredacted fields. Capturing state does not itself require those traits.
- Whether a record can move between threads, for example, depends on the captured values, not the original handles.
- The companion preserves the source type's visibility and the visibility of its struct fields. A public associated type cannot expose a private type. Capture a public value or implement the trait by hand.

Named, tuple, and unit structs and enum variants keep their shape. Omitted tuple fields change the positions of later fields.
An empty or entirely omitted record contains no placeholder fields. An enum without variants still has no possible value.
The macro uses the fields and variants that the compiler enables for this build.
It does not copy layout attributes or explicit enum discriminants. Do not rely on the record having the same memory layout as the source.

The derive supports Unicode and raw Rust identifiers. For example, `r#type` generates `typeSnapshot` by default.
The companion retains applicable documentation. With `into`, the capture method still documents omissions.

## Handle recursive and generic types

A snapshot must finish even if the source can refer back to itself.
If a field names `Self` or the source type directly, supply a finite summary or implement `Snapshot` yourself.
For aliases and qualified paths, Rust resolves the trait requirements. The last part of a type name alone does not identify recursion.
The derive does not automatically traverse cycles.

A type parameter can share the source type's name. The derive renames the parameter inside generated implementations, including bounds and custom capture paths.
It does not rename the source parameter or change the public companion type. It cannot rewrite tokens inside another macro.
If a type macro contains the conflicting parameter, give the parameter a distinct name or implement `Snapshot` yourself.

See the [snapshot examples and tests](../../crates/rca-macros/tests/snapshot_owned.rs) for custom capture, existing destinations, and generic records.
