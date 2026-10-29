# Owned Snapshot derive

The optional `macros` feature adds `#[derive(Snapshot)]`. The trait remains
available without macros, diagnostics or testing features. Importing
`rca::Snapshot` brings the trait and, when enabled, derive into their separate
Rust namespaces.

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

Every retained field is captured, including private fields. Default capture calls
its `Snapshot` implementation. There is no blanket Clone capture or implicit
capture of references, shared handles, locks, cells or task authority. Custom
capture requires both `#[snapshot(with = capture_function, value = OwnedType)]`
keys. The function takes a shared reference and runs exactly once, in field
declaration order. Multiple reads are not an atomic transaction.

Each retained field becomes one generic parameter on the companion, storing only
its captured value. Source lifetimes, generic bounds and omitted markers are not
copied into it. Name an inferred output with `<Importer as Snapshot>::Value` when
the explicit companion arguments would be cumbersome. The companion implements
Clone, PartialEq and Eq only when all stored fields do; Debug requires only the
unredacted fields to implement Debug. None of those traits is required to capture
a value. Auto-traits depend on the actual captured values.

`redact` changes Debug output to `"<redacted>"`, not the stored value or equality.
It does not hide direct field access. Source item and struct field visibility are
preserved. A private payload in a public associated value can still trigger Rust's
privacy error; capture a public owned value or supply a manual implementation.

`#[snapshot(into = ExistingHistory)]` uses an existing named-field struct or legal
struct alias. Generic type paths are accepted. Only named-field source structs
support `into`; omitted fields are absent from its constructor. The destination
owns its visibility and traits, so `redact` is rejected in this mode. `name` and
`into` cannot be combined. Rust checks missing, extra, inaccessible or incompatible
destination fields. If the default companion name collides, select `name` or
`into` explicitly.

Named, tuple and unit structs and enum variants retain their shapes. Omitted
tuple fields disappear, so output indices may differ from source indices. Empty
and all-omitted shapes have no dummy fields. Empty enums remain uninhabited.
The derive receives cfg-active fields/variants from the compiler and does not
evaluate configuration itself. Source layout attributes and explicit enum
discriminants are not copied: a companion does not promise the source ABI.
Raw identifiers and Unicode names are supported; `r#type` defaults to
`typeSnapshot`. Documentation is retained where meaningful, and omission reasons
are recorded in companion and capture-method documentation rather than a runtime
registry, including the capture method when `into` suppresses a companion.

Omission requires a non-empty reason. It removes compared data, not outstanding
assertion obligations: projected history is still projected coverage. It does
not acknowledge root updates or clear capture debt. Keep task, subscription and
other authority checks separate.

Custom capture must return a finite, independent owned projection. Obvious
references, raw pointers and function pointers in `value` are rejected. Aliases,
associated types and user code can still hide shared authority; the derive cannot
certify semantic independence. Bare `Self` or source-name recursion requires a
finite custom capture or manual implementation. Qualified paths and aliases remain
compiler trait obligations; a qualified name matching the source's final segment
does not establish recursion. Neither path promises automatic cycle traversal.
Type parameters may share the source item name. The derive renames only that
parameter inside generated impl syntax, including bounds and custom capture/value
paths; the source declaration and companion API are unchanged. Macro token bodies
are not expanded or rewritten: if a type macro carries that colliding parameter,
choose a distinct parameter name or write the Snapshot implementation manually.
Later source writes must not change captured history, and edits or clones of
expected history must not change source state or retained aliases. A custom Clone
that reintroduces shared mutable authority violates the same contract.

Pass `Snapshot::snapshot` to existing `TestStore::projected` or diagnostics capture
callbacks. Their capability requirements remain at those consumers; the derive
does not add a runtime capture hook, serializer or automatic TestStore constructor.
