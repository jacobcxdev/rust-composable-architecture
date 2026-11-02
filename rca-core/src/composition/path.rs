//! Defines borrowing state routes and owned action conversions.
//!
//! Paths retain concrete callbacks, not domain values. Borrowing routes preserve
//! the selected storage; computed routes instead produce owned values. Paths do
//! not enforce agreement between independent callbacks. Composition adapters
//! check the presence or semantic identity they need before using child authority.

use core::convert::Infallible;
use core::fmt;
use core::marker::PhantomData;

use super::keyed::{KeyedAction, KeyedState};

/// Limits derive-generated payload projection to supported storage containers.
mod sealed {
    /// Marks optional storage without exposing an extension point for projections.
    pub trait OptionalStatePayload {}

    /// Marks keyed storage without requiring bounds on its keys or children.
    pub trait KeyedStatePayload {}
}

/// Projects an optional state payload without inspecting its source spelling.
///
/// `StatePaths` uses this sealed contract for [`Option`] fields, including
/// aliases. It adds no bounds to the child state.
pub trait OptionalStatePayload: sealed::OptionalStatePayload {
    /// Identifies the optional child state.
    type Child;

    /// Borrows the child when present.
    #[must_use]
    fn as_optional(&self) -> Option<&Self::Child>;

    /// Mutably borrows the child when present.
    #[must_use]
    fn as_optional_mut(&mut self) -> Option<&mut Self::Child>;
}

impl<Child> sealed::OptionalStatePayload for Option<Child> {}

impl<Child> OptionalStatePayload for Option<Child> {
    type Child = Child;

    fn as_optional(&self) -> Option<&Self::Child> {
        self.as_ref()
    }

    fn as_optional_mut(&mut self) -> Option<&mut Self::Child> {
        self.as_mut()
    }
}

/// Projects a keyed state payload without inspecting its source spelling.
///
/// `StatePaths` uses this sealed contract for [`KeyedState`] fields, including
/// aliases. It adds no bounds to the identifier or child state.
pub trait KeyedStatePayload: sealed::KeyedStatePayload {
    /// Identifies each child.
    type Id;

    /// Identifies the child state.
    type Child;

    /// Borrows the keyed collection.
    #[must_use]
    fn as_keyed_state(&self) -> &KeyedState<Self::Id, Self::Child>;

    /// Mutably borrows the keyed collection.
    #[must_use]
    fn as_keyed_state_mut(&mut self) -> &mut KeyedState<Self::Id, Self::Child>;
}

impl<Id, Child> sealed::KeyedStatePayload for KeyedState<Id, Child> {}

impl<Id, Child> KeyedStatePayload for KeyedState<Id, Child> {
    type Id = Id;
    type Child = Child;

    fn as_keyed_state(&self) -> &KeyedState<Self::Id, Self::Child> {
        self
    }

    fn as_keyed_state_mut(&mut self) -> &mut KeyedState<Self::Id, Self::Child> {
        self
    }
}

/// Projects required child state through immutable and mutable borrows.
///
/// Both closures use higher-ranked bounds so each returned child borrow is tied
/// to the corresponding parent borrow. The path stores only its closures; it
/// stores no parent or child value and adds no `Clone` bound to either domain.
/// Projection performs no reflection, validation, allocation, or cloning.
/// Identity-aware composition checks that its mutable selection has the observed
/// child's semantic identity before reduction; this does not require the two
/// projections to return the same address.
///
/// # Examples
///
/// ```
/// use rca_core::StatePath;
///
/// struct Parent {
///     child: String,
/// }
///
/// let path = StatePath::new(
///     |state: &Parent| &state.child,
///     |state: &mut Parent| &mut state.child,
/// );
/// let mut state = Parent { child: "old".into() };
/// path.get_mut(&mut state).push_str(" value");
/// assert_eq!(path.get(&state), "old value");
/// ```
pub struct StatePath<Parent, Child, Get, GetMut> {
    /// Selects child storage while retaining the parent borrow's lifetime.
    get: Get,
    /// Selects exclusive child storage without retaining a borrow between calls.
    get_mut: GetMut,
    /// Records domain variance without owning values or inheriting their auto traits.
    marker: PhantomData<fn(Parent) -> Child>,
}

/// Projects an entire root state through its original storage.
///
/// Returns the existing [`StatePath`] carrier with `Copy`, higher-ranked
/// borrowing closures. No domain bounds, allocation or cloning are added.
///
/// # Examples
/// ```
/// let path = rca_core::identity_state_path::<String>();
/// let mut state = String::from("root");
/// path.get_mut(&mut state).push('!');
/// assert_eq!(path.get(&state), "root!");
/// ```
#[must_use]
pub fn identity_state_path<T>() -> StatePath<
    T,
    T,
    impl for<'a> Fn(&'a T) -> &'a T + Copy,
    impl for<'a> Fn(&'a mut T) -> &'a mut T + Copy,
> {
    StatePath::new(|root| root, |root| root)
}

/// Extracts and consumes an entire root action without cloning.
///
/// Returns the existing [`ActionPath`] carrier with `Copy` closures. Extraction
/// always returns `Some(root)` with the original borrow; embedding consumes the
/// supplied action unchanged. No domain bounds or allocation are added.
///
/// # Examples
/// ```
/// let path = rca_core::identity_action_path::<String>();
/// let action = path.embed(String::from("tap"));
/// assert!(std::ptr::eq(path.extract(&action).unwrap(), &action));
/// ```
#[must_use]
pub fn identity_action_path<T>()
-> ActionPath<T, T, impl for<'a> Fn(&'a T) -> Option<&'a T> + Copy, impl Fn(T) -> T + Copy> {
    ActionPath::new(|root| Some(root), |root| root)
}

/// Projects an entire root keyed collection through its original storage.
///
/// Returns the existing [`KeyedStatePath`] carrier with `Copy`, higher-ranked
/// borrowing closures. No identifier/state bounds, allocation or cloning are added.
///
/// # Examples
/// ```
/// let path = rca_core::identity_keyed_state_path::<u8, String>();
/// let mut rows = rca_core::KeyedState::new();
/// path.get_mut(&mut rows).insert(7, String::from("row"));
/// assert_eq!(path.get(&rows).get(&7).unwrap(), "row");
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "keeps the existing keyed carrier and inferred higher-ranked closure types"
)]
pub fn identity_keyed_state_path<Id, T>() -> KeyedStatePath<
    KeyedState<Id, T>,
    Id,
    T,
    impl for<'a> Fn(&'a KeyedState<Id, T>) -> &'a KeyedState<Id, T> + Copy,
    impl for<'a> Fn(&'a mut KeyedState<Id, T>) -> &'a mut KeyedState<Id, T> + Copy,
> {
    KeyedStatePath::new(|root| root, |root| root)
}

/// Extracts and consumes an entire root keyed action without cloning.
///
/// Returns the existing [`KeyedActionPath`] carrier with `Copy` closures.
/// Extraction always matches and borrows the original identifier/action fields;
/// embedding consumes both values unchanged. No domain bounds or allocation are added.
///
/// # Examples
/// ```
/// let path = rca_core::identity_keyed_action_path::<u8, String>();
/// let action = path.embed(7, String::from("tap"));
/// assert_eq!(path.extract(&action).unwrap(), (&7, &String::from("tap")));
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "keeps the existing keyed carrier and inferred higher-ranked closure types"
)]
pub fn identity_keyed_action_path<Id, T>() -> KeyedActionPath<
    KeyedAction<Id, T>,
    Id,
    T,
    impl for<'a> Fn(&'a KeyedAction<Id, T>) -> Option<&'a KeyedAction<Id, T>> + Copy,
    impl Fn(KeyedAction<Id, T>) -> KeyedAction<Id, T> + Copy,
> {
    KeyedActionPath::new(|root| Some(root), |root| root)
}

impl<Parent, Child, Get, GetMut> StatePath<Parent, Child, Get, GetMut>
where
    Get: for<'a> Fn(&'a Parent) -> &'a Child,
    GetMut: for<'a> Fn(&'a mut Parent) -> &'a mut Child,
{
    /// Creates a required state path from borrowing closures.
    #[must_use]
    pub const fn new(get: Get, get_mut: GetMut) -> Self {
        Self {
            get,
            get_mut,
            marker: PhantomData,
        }
    }

    /// Borrows child state from `parent`.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get<'a>(&self, parent: &'a Parent) -> &'a Child {
        (self.get)(parent)
    }

    /// Mutably borrows child state from `parent`.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get_mut<'a>(&self, parent: &'a mut Parent) -> &'a mut Child {
        (self.get_mut)(parent)
    }

    /// Transforms an independently captured payload and commits only success.
    ///
    /// Projection, capture and transform run once, in that order. `capture`
    /// must produce an owned value independent of the original for `Err` to
    /// leave it unchanged. Shared aliases, interior mutation and callback side
    /// effects are not rolled back. No panic rollback is promised. Success
    /// replaces the selected payload exactly once. No domain bounds are added.
    ///
    /// # Errors
    /// Returns the original typed transform error without committing its capture.
    ///
    /// # Panics
    /// Propagates projection, capture, transform and replaced-payload drop panics.
    pub fn try_modify_captured<E>(
        &self,
        parent: &mut Parent,
        capture: impl FnOnce(&Child) -> Child,
        transform: impl FnOnce(Child) -> Result<Child, E>,
    ) -> Result<(), E> {
        let child = self.get_mut(parent);
        let captured = capture(child);
        let replacement = transform(captured)?;
        *child = replacement;
        Ok(())
    }
}

impl<Parent, Child, Get, GetMut> Clone for StatePath<Parent, Child, Get, GetMut>
where
    Get: Clone,
    GetMut: Clone,
{
    fn clone(&self) -> Self {
        Self {
            get: self.get.clone(),
            get_mut: self.get_mut.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Child, Get, GetMut> Copy for StatePath<Parent, Child, Get, GetMut>
where
    Get: Copy,
    GetMut: Copy,
{
}

impl<Parent, Child, Get, GetMut> fmt::Debug for StatePath<Parent, Child, Get, GetMut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatePath").finish_non_exhaustive()
    }
}

/// Projects optional child state through immutable and mutable borrows.
///
/// Both closures use higher-ranked bounds tying returned child borrows to the
/// parent borrow. The path stores only its closures and adds no `Clone` bound
/// to either state type. A missing child is reported as `None`.
///
/// # Examples
///
/// ```
/// use rca_core::OptionalStatePath;
///
/// struct Parent {
///     child: Option<String>,
/// }
///
/// let path = OptionalStatePath::new(
///     |state: &Parent| state.child.as_ref(),
///     |state: &mut Parent| state.child.as_mut(),
/// );
/// let state = Parent { child: None };
/// assert!(path.get(&state).is_none());
/// ```
pub struct OptionalStatePath<Parent, Child, Get, GetMut> {
    /// Selects an existing child without creating storage for an absent value.
    get: Get,
    /// Selects exclusive storage only when the child exists.
    get_mut: GetMut,
    /// Records domain variance without owning values or inheriting their auto traits.
    marker: PhantomData<fn(Parent) -> Child>,
}

impl<Parent, Child, Get, GetMut> OptionalStatePath<Parent, Child, Get, GetMut>
where
    Get: for<'a> Fn(&'a Parent) -> Option<&'a Child>,
    GetMut: for<'a> Fn(&'a mut Parent) -> Option<&'a mut Child>,
{
    /// Creates an optional state path from borrowing closures.
    #[must_use]
    pub const fn new(get: Get, get_mut: GetMut) -> Self {
        Self {
            get,
            get_mut,
            marker: PhantomData,
        }
    }

    /// Borrows child state when it is present.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get<'a>(&self, parent: &'a Parent) -> Option<&'a Child> {
        (self.get)(parent)
    }

    /// Mutably borrows child state when it is present.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get_mut<'a>(&self, parent: &'a mut Parent) -> Option<&'a mut Child> {
        (self.get_mut)(parent)
    }

    /// Transforms an independent capture when the optional payload exists.
    ///
    /// Returns `Ok(false)` for absence without capture, transform or write.
    /// Otherwise projection, capture and transform run once; `Ok(true)` commits
    /// one replacement. Independence is the caller's obligation: shared aliases,
    /// interior mutation and callback side effects have no rollback guarantee.
    /// No panic rollback or domain bounds are added.
    ///
    /// # Errors
    /// Returns the original typed transform error without committing its capture.
    ///
    /// # Panics
    /// Propagates projection, capture, transform and replaced-payload drop panics.
    pub fn try_modify_captured<E>(
        &self,
        parent: &mut Parent,
        capture: impl FnOnce(&Child) -> Child,
        transform: impl FnOnce(Child) -> Result<Child, E>,
    ) -> Result<bool, E> {
        let Some(child) = self.get_mut(parent) else {
            return Ok(false);
        };
        let captured = capture(child);
        let replacement = transform(captured)?;
        *child = replacement;
        Ok(true)
    }
}

impl<Parent, Child, Get, GetMut> Clone for OptionalStatePath<Parent, Child, Get, GetMut>
where
    Get: Clone,
    GetMut: Clone,
{
    fn clone(&self) -> Self {
        Self {
            get: self.get.clone(),
            get_mut: self.get_mut.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Child, Get, GetMut> Copy for OptionalStatePath<Parent, Child, Get, GetMut>
where
    Get: Copy,
    GetMut: Copy,
{
}

impl<Parent, Child, Get, GetMut> fmt::Debug for OptionalStatePath<Parent, Child, Get, GetMut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OptionalStatePath").finish_non_exhaustive()
    }
}

/// Projects one child state case through immutable and mutable borrows.
///
/// Both closures use higher-ranked bounds tying returned child borrows to the
/// parent borrow. The path stores only its closures and uses ordinary pattern
/// matching, without runtime enum metadata or layout inspection.
///
/// # Examples
///
/// ```
/// use rca_core::CaseStatePath;
///
/// enum State {
///     Child(String),
///     Parent,
/// }
///
/// let path = CaseStatePath::new(
///     |state: &State| match state {
///         State::Child(child) => Some(child),
///         State::Parent => None,
///     },
///     |state: &mut State| match state {
///         State::Child(child) => Some(child),
///         State::Parent => None,
///     },
/// );
/// assert!(path.get(&State::Parent).is_none());
/// ```
pub struct CaseStatePath<Parent, Child, Get, GetMut> {
    /// Borrows the payload only when the selected case matches.
    get: Get,
    /// Borrows exclusive payload storage only when the selected case matches.
    get_mut: GetMut,
    /// Records domain variance without owning values or inheriting their auto traits.
    marker: PhantomData<fn(Parent) -> Child>,
}

impl<Parent, Child, Get, GetMut> CaseStatePath<Parent, Child, Get, GetMut>
where
    Get: for<'a> Fn(&'a Parent) -> Option<&'a Child>,
    GetMut: for<'a> Fn(&'a mut Parent) -> Option<&'a mut Child>,
{
    /// Creates a case state path from borrowing closures.
    #[must_use]
    pub const fn new(get: Get, get_mut: GetMut) -> Self {
        Self {
            get,
            get_mut,
            marker: PhantomData,
        }
    }

    /// Borrows child state when the case matches.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get<'a>(&self, parent: &'a Parent) -> Option<&'a Child> {
        (self.get)(parent)
    }

    /// Transforms an independent capture when the selected case matches.
    ///
    /// Projection runs once. A wrong case returns `Ok(false)` without capture,
    /// transform or write. A match captures and transforms once, committing one
    /// replacement only on `Ok` and returning `Ok(true)`. Independence is the
    /// caller's obligation: shared aliases, interior mutation and callback side
    /// effects have no rollback guarantee. No panic rollback or domain bounds
    /// are added. This borrowed path grants no runtime scope authority.
    ///
    /// # Errors
    /// Returns the original typed transform error without committing its capture.
    ///
    /// # Panics
    /// Propagates projection, capture, transform and replaced-payload drop panics.
    pub fn try_modify_captured<E>(
        &self,
        parent: &mut Parent,
        capture: impl FnOnce(&Child) -> Child,
        transform: impl FnOnce(Child) -> Result<Child, E>,
    ) -> Result<bool, E> {
        let Some(child) = self.get_mut(parent) else {
            return Ok(false);
        };
        let captured = capture(child);
        let replacement = transform(captured)?;
        *child = replacement;
        Ok(true)
    }

    /// Mutably borrows child state when the case matches.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get_mut<'a>(&self, parent: &'a mut Parent) -> Option<&'a mut Child> {
        (self.get_mut)(parent)
    }
}

impl<Parent, Child, Get, GetMut> Clone for CaseStatePath<Parent, Child, Get, GetMut>
where
    Get: Clone,
    GetMut: Clone,
{
    fn clone(&self) -> Self {
        Self {
            get: self.get.clone(),
            get_mut: self.get_mut.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Child, Get, GetMut> Copy for CaseStatePath<Parent, Child, Get, GetMut>
where
    Get: Copy,
    GetMut: Copy,
{
}

impl<Parent, Child, Get, GetMut> fmt::Debug for CaseStatePath<Parent, Child, Get, GetMut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaseStatePath").finish_non_exhaustive()
    }
}

/// Extracts borrowed child actions and embeds owned child actions.
///
/// The extractor uses a higher-ranked bound tying a returned child borrow to
/// the parent action borrow. Embedding consumes the child action and returns an
/// owned parent action. The path stores only its closures and adds no `Clone`
/// bound to either action type.
///
/// # Examples
///
/// ```
/// use rca_core::ActionPath;
///
/// enum Action {
///     Child(Vec<u8>),
///     Parent,
/// }
///
/// let path = ActionPath::new(
///     |action: &Action| match action {
///         Action::Child(child) => Some(child),
///         Action::Parent => None,
///     },
///     Action::Child,
/// );
/// let action = Action::Child(vec![1, 2]);
/// assert_eq!(path.extract(&action).unwrap(), &[1, 2]);
/// assert!(matches!(path.embed(vec![3]), Action::Child(bytes) if bytes == [3]));
/// ```
pub struct ActionPath<Parent, Child, Extract, Embed> {
    /// Borrows the selected payload without taking ownership of its parent action.
    extract: Extract,
    /// Consumes a child output to construct its parent action.
    embed: Embed,
    /// Records domain variance without owning values or inheriting their auto traits.
    marker: PhantomData<fn(Parent) -> Child>,
}

impl<Parent, Child, Extract, Embed> ActionPath<Parent, Child, Extract, Embed>
where
    Extract: for<'a> Fn(&'a Parent) -> Option<&'a Child>,
    Embed: Fn(Child) -> Parent,
{
    /// Creates an action path from extraction and embedding closures.
    #[must_use]
    pub const fn new(extract: Extract, embed: Embed) -> Self {
        Self {
            extract,
            embed,
            marker: PhantomData,
        }
    }

    /// Borrows a child action when the parent action matches.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied extraction closure panics.
    #[must_use]
    pub fn extract<'a>(&self, parent: &'a Parent) -> Option<&'a Child> {
        (self.extract)(parent)
    }

    /// Embeds an owned child action into an owned parent action.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied embedding closure panics.
    #[must_use]
    pub fn embed(&self, child: Child) -> Parent {
        (self.embed)(child)
    }

    /// Composes stored extraction forward and owned embedding in reverse.
    ///
    /// Either missing case returns `None`. No payload is cloned or synthesised.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{some_path, ok_path};
    /// let path = some_path::<Result<String, ()>>().then(ok_path());
    /// let root = path.embed(String::from("value"));
    /// assert_eq!(path.extract(&root).map(String::as_str), Some("value"));
    /// assert!(path.extract(&None).is_none());
    /// assert!(path.extract(&Some(Err(()))).is_none());
    /// ```
    ///
    /// Intermediate domains must agree statically:
    ///
    /// ```compile_fail
    /// use rca_core::{some_path, ok_path};
    /// let _ = some_path::<String>().then(ok_path::<u32, ()>());
    /// ```
    #[must_use]
    pub fn then<Next>(self, next: Next) -> ComposedActionPath<Self, Next>
    where
        Next: StoredActionPath<Root = Child>,
    {
        StoredActionPath::then(self, next)
    }

    /// Lifts the root into an explicit optional layer.
    ///
    /// A missing root returns outer `None`; a matched optional payload remains
    /// `Some(&None)`. Embedding always constructs a present root.
    #[must_use]
    pub fn lift_optional(self) -> LiftedActionPath<Self> {
        StoredActionPath::lift_optional(self)
    }

    /// Copies only output-routing metadata for an effect that outlives reduction.
    pub(crate) fn clone_embedder(&self) -> Embed
    where
        Embed: Clone,
    {
        self.embed.clone()
    }
}

/// Embeds an owned stored-case payload without cloning the payload.
///
/// Closures implement this protocol directly; composed and lifted metadata
/// preserve conditional `Clone` independently of extraction captures.
pub trait ActionEmbedding<Root, Value> {
    /// Consumes a payload to construct its root.
    ///
    /// # Panics
    ///
    /// May panic if the caller's embedding closure panics.
    #[must_use]
    fn embed(&self, value: Value) -> Root;
}

impl<Root, Value, Embed: Fn(Value) -> Root> ActionEmbedding<Root, Value> for Embed {
    fn embed(&self, value: Value) -> Root {
        self(value)
    }
}

/// Borrows stored cases and composes their owned embedding metadata.
///
/// This protocol is confined to stored action paths. The borrow-time `Self`
/// bound keeps intermediate local domains valid without requiring `'static`.
/// Use inherent methods on the concrete paths for ordinary composition.
pub trait StoredActionPath: Sized {
    /// The complete action domain.
    type Root;
    /// The selected stored payload domain.
    type Value;
    /// Owned embedding metadata, independent of extraction captures.
    type Embedder: ActionEmbedding<Self::Root, Self::Value>;

    /// Borrows a stored payload when every selected case matches.
    ///
    /// # Panics
    ///
    /// May panic if the caller's extraction closure panics.
    #[must_use]
    fn extract<'a>(&self, root: &'a Self::Root) -> Option<&'a Self::Value>
    where
        Self: 'a;

    /// Embeds an owned payload through the selected cases.
    ///
    /// # Panics
    ///
    /// May panic if the caller's embedding closure panics.
    #[must_use]
    fn embed(&self, value: Self::Value) -> Self::Root;

    /// Appends another stored route without a fixed nesting-depth limit.
    #[must_use]
    fn then<Next: StoredActionPath<Root = Self::Value>>(
        self,
        next: Next,
    ) -> ComposedActionPath<Self, Next> {
        ComposedActionPath {
            outer: self,
            inner: next,
            marker: PhantomData,
        }
    }

    /// Adds a present optional root without flattening optional payloads.
    #[must_use]
    fn lift_optional(self) -> LiftedActionPath<Self> {
        LiftedActionPath { path: self }
    }

    /// Converts an owned route for existing effect-bearing scope consumers.
    ///
    /// The conversion clones embedding captures once; extraction captures are
    /// moved unchanged. Only this owned boundary requires `'static` metadata
    /// and domains. The returned embedder is `Clone` when its captures are.
    /// Existing consumers retain their own `Send`/`Sync` transport bounds.
    ///
    /// # Panics
    ///
    /// May panic if a caller-supplied embedding capture's `Clone` panics.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "Preserves typed closures for existing scope consumers"
    )]
    fn into_action_path(
        self,
    ) -> ActionPath<
        Self::Root,
        Self::Value,
        impl for<'a> Fn(&'a Self::Root) -> Option<&'a Self::Value>,
        impl Fn(Self::Value) -> Self::Root + Clone,
    >
    where
        Self: CloneActionPath + 'static,
        Self::Root: 'static,
        Self::Value: 'static,
        Self::Embedder: Clone,
    {
        let embed = CloneActionPath::clone_embedder(&self);
        ActionPath::new(
            move |root| self.extract(root),
            move |value| embed.embed(value),
        )
    }
}

/// Clones stored-path embedding metadata without cloning extraction captures or payloads.
///
/// This separate capability leaves locally captured, non-cloneable paths usable
/// for borrowing, embedding and further composition.
pub trait CloneActionPath: StoredActionPath {
    /// Clones only the captures used to embed an owned payload.
    ///
    /// # Panics
    ///
    /// May panic if a caller-supplied capture's `Clone` panics.
    #[must_use]
    fn clone_embedder(&self) -> Self::Embedder;
}

impl<Root, Value, Extract, Embed> StoredActionPath for ActionPath<Root, Value, Extract, Embed>
where
    Extract: for<'a> Fn(&'a Root) -> Option<&'a Value>,
    Embed: Fn(Value) -> Root,
{
    type Root = Root;
    type Value = Value;
    type Embedder = Embed;

    fn extract<'a>(&self, root: &'a Root) -> Option<&'a Value>
    where
        Self: 'a,
    {
        Self::extract(self, root)
    }

    fn embed(&self, value: Value) -> Root {
        Self::embed(self, value)
    }
}

impl<Root, Value, Extract, Embed> CloneActionPath for ActionPath<Root, Value, Extract, Embed>
where
    Extract: for<'a> Fn(&'a Root) -> Option<&'a Value>,
    Embed: Fn(Value) -> Root + Clone,
{
    fn clone_embedder(&self) -> Embed {
        self.embed.clone()
    }
}

/// Composes stored paths recursively without requiring static local domains.
///
/// Routes remain composable and liftable at any depth. Conversion to the
/// closure-based [`ActionPath`] is explicit at the owned scope boundary:
///
/// # Examples
///
/// ```
/// use rca_core::{some_path, ok_path};
/// let route = some_path::<Result<Option<String>, ()>>()
///     .then(ok_path())
///     .then(some_path())
///     .lift_optional();
/// let root = route.embed(String::from("nested"));
/// assert_eq!(route.extract(&root).map(String::as_str), Some("nested"));
/// let owned = route.into_action_path();
/// assert_eq!(owned.embed(String::from("value")), Some(Some(Ok(Some(String::from("value"))))));
/// ```
pub struct ComposedActionPath<Outer: StoredActionPath, Inner> {
    /// Selects the intermediate payload and reconstructs the outer action.
    outer: Outer,
    /// Selects within the intermediate payload and embeds the innermost value first.
    inner: Inner,
    /// Records the intermediate output type without retaining any payload.
    marker: PhantomData<fn() -> Outer::Value>,
}

/// Lifts a stored action route into a present optional root.
///
/// A missing root is not a match for a stored optional payload. Stateless
/// lifting retains `Copy` metadata without requiring a copyable payload.
///
/// # Examples
///
/// ```
/// use rca_core::some_path;
/// let route = some_path::<Option<String>>().lift_optional();
/// assert!(route.extract(&None).is_none());
/// let root = route.embed(None);
/// assert_eq!(root, Some(Some(None)));
/// assert_eq!(route.extract(&root), Some(&None));
/// let owned = route.into_action_path();
/// assert_eq!(owned.embed(None), Some(Some(None)));
/// ```
///
/// Owned conversion does not make borrowed local payloads static:
///
/// ```compile_fail
/// use rca_core::some_path;
/// let text = String::from("local");
/// let owned = some_path::<&str>().lift_optional().into_action_path();
/// let _root = owned.embed(text.as_str());
/// ```
#[derive(Clone, Copy)]
pub struct LiftedActionPath<Path> {
    /// Operates inside a present root without flattening its own optional payloads.
    path: Path,
}

/// Holds composed embedding captures without retaining extraction captures.
pub struct ComposedActionEmbedding<Root, Middle, Value, Outer, Inner> {
    /// Consumes the intermediate action produced by the inner embedder.
    outer: Outer,
    /// Consumes the final output before the outer embedder runs.
    inner: Inner,
    /// Records domain variance without retaining output or intermediate values.
    marker: PhantomData<fn(Value) -> (Root, Middle)>,
}

/// Holds optional-root embedding captures without retaining extraction captures.
#[derive(Clone, Copy)]
pub struct LiftedActionEmbedding<Embed> {
    /// Builds the inner root before wrapping it in a present optional root.
    embed: Embed,
}

impl<Root, Middle, Value, Outer, Inner> ActionEmbedding<Root, Value>
    for ComposedActionEmbedding<Root, Middle, Value, Outer, Inner>
where
    Outer: ActionEmbedding<Root, Middle>,
    Inner: ActionEmbedding<Middle, Value>,
{
    fn embed(&self, value: Value) -> Root {
        self.outer.embed(self.inner.embed(value))
    }
}

impl<Root, Value, Embed: ActionEmbedding<Root, Value>> ActionEmbedding<Option<Root>, Value>
    for LiftedActionEmbedding<Embed>
{
    fn embed(&self, value: Value) -> Option<Root> {
        Some(self.embed.embed(value))
    }
}

impl<Outer, Inner> StoredActionPath for ComposedActionPath<Outer, Inner>
where
    Outer: StoredActionPath,
    Inner: StoredActionPath<Root = Outer::Value>,
{
    type Root = Outer::Root;
    type Value = Inner::Value;
    type Embedder = ComposedActionEmbedding<
        Outer::Root,
        Outer::Value,
        Inner::Value,
        Outer::Embedder,
        Inner::Embedder,
    >;

    fn extract<'a>(&self, root: &'a Self::Root) -> Option<&'a Self::Value>
    where
        Self: 'a,
    {
        self.outer
            .extract(root)
            .and_then(|child| self.inner.extract(child))
    }

    fn embed(&self, value: Self::Value) -> Self::Root {
        self.outer.embed(self.inner.embed(value))
    }
}

impl<Outer, Inner> CloneActionPath for ComposedActionPath<Outer, Inner>
where
    Outer: CloneActionPath,
    Inner: CloneActionPath<Root = Outer::Value>,
{
    fn clone_embedder(&self) -> Self::Embedder {
        ComposedActionEmbedding {
            outer: self.outer.clone_embedder(),
            inner: self.inner.clone_embedder(),
            marker: PhantomData,
        }
    }
}

impl<Path: StoredActionPath> StoredActionPath for LiftedActionPath<Path> {
    type Root = Option<Path::Root>;
    type Value = Path::Value;
    type Embedder = LiftedActionEmbedding<Path::Embedder>;

    fn extract<'a>(&self, root: &'a Self::Root) -> Option<&'a Self::Value>
    where
        Self: 'a,
    {
        root.as_ref().and_then(|root| self.path.extract(root))
    }

    fn embed(&self, value: Self::Value) -> Self::Root {
        Some(self.path.embed(value))
    }
}

impl<Path: CloneActionPath> CloneActionPath for LiftedActionPath<Path> {
    fn clone_embedder(&self) -> Self::Embedder {
        LiftedActionEmbedding {
            embed: self.path.clone_embedder(),
        }
    }
}

/// Exposes the same inherent operations on both recursive carriers without erasure.
macro_rules! stored_path_methods {
    () => {
        /// Borrows the selected payload without cloning or synthesising it.
        ///
        /// # Panics
        ///
        /// May panic if a caller-supplied extractor panics.
        #[must_use]
        pub fn extract<'a>(
            &self,
            root: &'a <Self as StoredActionPath>::Root,
        ) -> Option<&'a <Self as StoredActionPath>::Value>
        where
            Self: 'a,
        {
            StoredActionPath::extract(self, root)
        }

        /// Embeds a payload through every selected case in reverse order.
        ///
        /// # Panics
        ///
        /// May panic if a caller-supplied embedder panics.
        #[must_use]
        pub fn embed(
            &self,
            value: <Self as StoredActionPath>::Value,
        ) -> <Self as StoredActionPath>::Root {
            StoredActionPath::embed(self, value)
        }

        /// Appends another stored route without requiring static local domains.
        #[must_use]
        pub fn then<Next: StoredActionPath<Root = <Self as StoredActionPath>::Value>>(
            self,
            next: Next,
        ) -> ComposedActionPath<Self, Next> {
            StoredActionPath::then(self, next)
        }

        /// Adds a present optional root without flattening optional payloads.
        #[must_use]
        pub fn lift_optional(self) -> LiftedActionPath<Self> {
            StoredActionPath::lift_optional(self)
        }

        /// Converts an owned route for existing effect-bearing scope consumers.
        ///
        /// Only this boundary requires static metadata and domains. It clones
        /// embedding captures once, moves extraction captures, and preserves
        /// conditional embedder `Clone` without cloning a payload.
        ///
        /// # Panics
        ///
        /// May panic if a caller-supplied embedding capture's `Clone` panics.
        #[must_use]
        pub fn into_action_path(
            self,
        ) -> ActionPath<
            <Self as StoredActionPath>::Root,
            <Self as StoredActionPath>::Value,
            impl for<'a> Fn(
                &'a <Self as StoredActionPath>::Root,
            ) -> Option<&'a <Self as StoredActionPath>::Value>,
            impl Fn(<Self as StoredActionPath>::Value) -> <Self as StoredActionPath>::Root + Clone,
        >
        where
            Self: CloneActionPath + 'static,
            <Self as StoredActionPath>::Root: 'static,
            <Self as StoredActionPath>::Value: 'static,
            <Self as StoredActionPath>::Embedder: Clone,
        {
            StoredActionPath::into_action_path(self)
        }
    };
}

impl<Outer: StoredActionPath, Inner: StoredActionPath<Root = Outer::Value>>
    ComposedActionPath<Outer, Inner>
{
    stored_path_methods!();
}

impl<Path: StoredActionPath> LiftedActionPath<Path> {
    stored_path_methods!();
}

#[expect(
    clippy::expl_impl_clone_on_copy,
    reason = "Deriving Clone/Copy would require the intermediate payload to implement those traits"
)]
impl<Outer: StoredActionPath + Clone, Inner: Clone> Clone for ComposedActionPath<Outer, Inner> {
    fn clone(&self) -> Self {
        Self {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            marker: PhantomData,
        }
    }
}

impl<Outer: StoredActionPath + Copy, Inner: Copy> Copy for ComposedActionPath<Outer, Inner> {}

impl<Root, Middle, Value, Outer: Clone, Inner: Clone> Clone
    for ComposedActionEmbedding<Root, Middle, Value, Outer, Inner>
{
    fn clone(&self) -> Self {
        Self {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            marker: PhantomData,
        }
    }
}

impl<Root, Middle, Value, Outer: Copy, Inner: Copy> Copy
    for ComposedActionEmbedding<Root, Middle, Value, Outer, Inner>
{
}

impl<Outer: StoredActionPath, Inner> fmt::Debug for ComposedActionPath<Outer, Inner> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposedActionPath").finish_non_exhaustive()
    }
}

impl<Path> fmt::Debug for LiftedActionPath<Path> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiftedActionPath").finish_non_exhaustive()
    }
}

impl<Root, Middle, Value, Outer, Inner> fmt::Debug
    for ComposedActionEmbedding<Root, Middle, Value, Outer, Inner>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposedActionEmbedding")
            .finish_non_exhaustive()
    }
}

impl<Embed> fmt::Debug for LiftedActionEmbedding<Embed> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiftedActionEmbedding")
            .finish_non_exhaustive()
    }
}

/// Borrows the present payload and embeds it in `Some`.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "Returns the existing typed path without erasure"
)]
pub fn some_path<Value>() -> ActionPath<
    Option<Value>,
    Value,
    impl for<'a> Fn(&'a Option<Value>) -> Option<&'a Value> + Copy,
    impl Fn(Value) -> Option<Value> + Copy,
> {
    ActionPath::new(Option::as_ref, Some)
}

/// Matches `None` as unit and embeds unit in `None`.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "Returns the existing typed path without erasure"
)]
pub fn none_path<Value>() -> ActionPath<
    Option<Value>,
    (),
    impl for<'a> Fn(&'a Option<Value>) -> Option<&'a ()> + Copy,
    impl Fn(()) -> Option<Value> + Copy,
> {
    ActionPath::new(
        |root: &Option<Value>| root.is_none().then_some(&()),
        |()| None,
    )
}

/// Borrows a successful payload and embeds it in `Ok`.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "Returns the existing typed path without erasure"
)]
pub fn ok_path<Value, Error>() -> ActionPath<
    Result<Value, Error>,
    Value,
    impl for<'a> Fn(&'a Result<Value, Error>) -> Option<&'a Value> + Copy,
    impl Fn(Value) -> Result<Value, Error> + Copy,
> {
    ActionPath::new(|root: &Result<Value, Error>| root.as_ref().ok(), Ok)
}

/// Borrows an error payload and embeds it in `Err`.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "Returns the existing typed path without erasure"
)]
pub fn err_path<Value, Error>() -> ActionPath<
    Result<Value, Error>,
    Error,
    impl for<'a> Fn(&'a Result<Value, Error>) -> Option<&'a Error> + Copy,
    impl Fn(Error) -> Result<Value, Error> + Copy,
> {
    ActionPath::new(|root: &Result<Value, Error>| root.as_ref().err(), Err)
}

/// Borrows and embeds unit without synthesising a payload.
#[must_use]
pub fn unit_path()
-> ActionPath<(), (), impl for<'a> Fn(&'a ()) -> Option<&'a ()> + Copy, impl Fn(()) + Copy> {
    ActionPath::new(|root: &()| Some(root), |()| ())
}

/// Extracts owned computed values with caller-defined validation and embedding.
///
/// Unlike [`ActionPath`], extraction synthesises an owned value, not a stored
/// mutable projection. These partial transformations need not be inverse optics:
/// callers define raw validation, parsing errors and normalisation in closures.
/// No `Clone`, `Send` or `'static` bound is imposed on their domains or captures.
///
/// # Examples
///
/// Text may normalise on embedding, while invalid text retains its parse error:
///
/// ```
/// use rca_core::ComputedPath;
/// let decimal = ComputedPath::new(
///     |text: &String| text.parse::<u32>(),
///     |value: u32| value.to_string(),
/// );
/// assert_eq!(decimal.embed(decimal.extract(&String::from("007")).unwrap()), "7");
/// assert!(decimal.extract(&String::from("invalid")).is_err());
/// ```
///
/// Computed values cannot lend mutable storage that does not exist:
///
/// ```compile_fail
/// use rca_core::constant_path;
/// let path = constant_path(|| String::from("fresh"));
/// path.get_mut(&mut ());
/// ```
pub struct ComputedPath<Root, Value, Error, Extract, Embed> {
    /// Produces an owned result; it cannot lend storage for mutable projection.
    extract: Extract,
    /// Consumes an owned value using the caller's normalisation policy.
    embed: Embed,
    /// Records domains and errors without owning their values or auto-trait bounds.
    marker: PhantomData<fn(Root) -> Result<Value, Error>>,
}

impl<Root, Value, Error, Extract, Embed> ComputedPath<Root, Value, Error, Extract, Embed>
where
    Extract: Fn(&Root) -> Result<Value, Error>,
    Embed: Fn(Value) -> Root,
{
    /// Creates a partial transformation from caller-supplied closures.
    #[must_use]
    pub const fn new(extract: Extract, embed: Embed) -> Self {
        Self {
            extract,
            embed,
            marker: PhantomData,
        }
    }

    /// Extracts one owned value from a borrowed root.
    ///
    /// # Errors
    ///
    /// Returns the caller's validation or conversion error unchanged.
    ///
    /// # Panics
    ///
    /// Panics only if the caller-supplied extractor panics.
    pub fn extract(&self, root: &Root) -> Result<Value, Error> {
        (self.extract)(root)
    }

    /// Consumes the value to construct a root with caller-defined normalisation.
    ///
    /// # Panics
    ///
    /// Panics only if the caller-supplied embedder panics.
    #[must_use]
    pub fn embed(&self, value: Value) -> Root {
        (self.embed)(value)
    }

    /// Composes owned conversions, preserving errors and short-circuiting failures.
    ///
    /// Both paths use the same caller-chosen error type. Distinct errors can be
    /// mapped explicitly into the caller's enum before composition. Embedding
    /// applies the inner recipe before the outer, not a universal inverse law.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "Opaque closures preserve local captures and typed errors"
    )]
    pub fn then<NextValue, NextExtract, NextEmbed>(
        self,
        next: ComputedPath<Value, NextValue, Error, NextExtract, NextEmbed>,
    ) -> ComputedPath<
        Root,
        NextValue,
        Error,
        impl Fn(&Root) -> Result<NextValue, Error>,
        impl Fn(NextValue) -> Root,
    >
    where
        NextExtract: Fn(&Value) -> Result<NextValue, Error>,
        NextEmbed: Fn(NextValue) -> Value,
    {
        ComputedPath::new(
            move |root| (self.extract)(root).and_then(|value| (next.extract)(&value)),
            move |value| (self.embed)((next.embed)(value)),
        )
    }
}

impl<Root, Value, Error, Extract, Embed> fmt::Debug
    for ComputedPath<Root, Value, Error, Extract, Embed>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComputedPath").finish_non_exhaustive()
    }
}

/// Synthesises a fresh owned value on each unit extraction.
///
/// `factory` runs exactly once per extraction and never during embedding.
/// Embedding drops the supplied value and returns unit; it is not an inverse
/// for arbitrary values. The factory can borrow caller-local state.
///
/// # Examples
///
/// ```
/// use rca_core::constant_path;
/// use std::cell::Cell;
/// let calls = Cell::new(0);
/// let path = constant_path(|| {
///     calls.set(calls.get() + 1);
///     Box::new(calls.get())
/// });
/// let value = path.extract(&()).unwrap();
/// assert_eq!(*value, 1);
/// let () = path.embed(value);
/// assert_eq!(calls.get(), 1);
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "Keeps the factory capture concrete and caller-local"
)]
pub fn constant_path<Value>(
    factory: impl Fn() -> Value,
) -> ComputedPath<(), Value, Infallible, impl Fn(&()) -> Result<Value, Infallible>, impl Fn(Value)>
{
    ComputedPath::new(move |&()| Ok(factory()), |_: Value| ())
}

impl<Parent, Child, Get, GetMut> StatePath<Parent, Child, Get, GetMut> {
    /// Transfers read metadata and drops the unused mutable-projection captures.
    pub(crate) fn into_get(self) -> Get {
        self.get
    }
}

impl<Parent, Child, Get, GetMut> OptionalStatePath<Parent, Child, Get, GetMut> {
    /// Transfers presence-aware read metadata without retaining mutation captures.
    pub(crate) fn into_get(self) -> Get {
        self.get
    }
}

impl<Parent, Child, Get, GetMut> CaseStatePath<Parent, Child, Get, GetMut> {
    /// Transfers case-aware read metadata without retaining mutation captures.
    pub(crate) fn into_get(self) -> Get {
        self.get
    }
}

impl<Parent, Child, Extract, Embed> ActionPath<Parent, Child, Extract, Embed> {
    /// Transfers owned output routing and drops the unused extraction captures.
    pub(crate) fn into_embedder(self) -> Embed {
        self.embed
    }
}

impl<Parent, Id, Child, Get, GetMut> KeyedStatePath<Parent, Id, Child, Get, GetMut> {
    /// Transfers collection read metadata without retaining mutation captures.
    pub(crate) fn into_get(self) -> Get {
        self.get
    }
}

impl<Parent, Child, Extract, Embed> Clone for ActionPath<Parent, Child, Extract, Embed>
where
    Extract: Clone,
    Embed: Clone,
{
    fn clone(&self) -> Self {
        Self {
            extract: self.extract.clone(),
            embed: self.embed.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Child, Extract, Embed> Copy for ActionPath<Parent, Child, Extract, Embed>
where
    Extract: Copy,
    Embed: Copy,
{
}

impl<Parent, Child, Extract, Embed> fmt::Debug for ActionPath<Parent, Child, Extract, Embed> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionPath").finish_non_exhaustive()
    }
}

/// Projects a keyed child collection through immutable and mutable borrows.
///
/// Both closures use higher-ranked bounds so each returned collection borrow
/// is tied to the corresponding parent borrow. The path stores only its
/// closures and adds no `Clone` bound to the identifier or either state type.
/// When used by keyed composition, the mutable selection must retain the observed
/// child's insertion lifetime. Equal identifiers in independent collections do
/// not grant the same child authority.
///
/// # Examples
///
/// ```
/// use rca_core::{KeyedState, KeyedStatePath};
///
/// struct Parent {
///     rows: KeyedState<u64, String>,
/// }
///
/// let path = KeyedStatePath::new(
///     |state: &Parent| &state.rows,
///     |state: &mut Parent| &mut state.rows,
/// );
/// let mut state = Parent { rows: KeyedState::new() };
/// path.get_mut(&mut state).insert(7, String::from("seven"));
/// assert_eq!(path.get(&state).get(&7).map(String::as_str), Some("seven"));
/// ```
pub struct KeyedStatePath<Parent, Id, Child, Get, GetMut> {
    /// Selects the collection whose membership the composition adapter inspects.
    get: Get,
    /// Selects exclusive collection storage for child reduction or mutation.
    get_mut: GetMut,
    /// Records domains without storing keys, child values or their auto-trait bounds.
    marker: PhantomData<fn(Parent) -> (Id, Child)>,
}

impl<Parent, Id, Child, Get, GetMut> KeyedStatePath<Parent, Id, Child, Get, GetMut>
where
    Get: for<'a> Fn(&'a Parent) -> &'a KeyedState<Id, Child>,
    GetMut: for<'a> Fn(&'a mut Parent) -> &'a mut KeyedState<Id, Child>,
{
    /// Creates a keyed state path from borrowing closures.
    #[must_use]
    pub const fn new(get: Get, get_mut: GetMut) -> Self {
        Self {
            get,
            get_mut,
            marker: PhantomData,
        }
    }

    /// Borrows the keyed child collection from `parent`.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get<'a>(&self, parent: &'a Parent) -> &'a KeyedState<Id, Child> {
        (self.get)(parent)
    }

    /// Mutably borrows the keyed child collection from `parent`.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied projection closure panics.
    #[must_use]
    pub fn get_mut<'a>(&self, parent: &'a mut Parent) -> &'a mut KeyedState<Id, Child> {
        (self.get_mut)(parent)
    }
}

impl<Parent, Id, Child, Get, GetMut> Clone for KeyedStatePath<Parent, Id, Child, Get, GetMut>
where
    Get: Clone,
    GetMut: Clone,
{
    fn clone(&self) -> Self {
        Self {
            get: self.get.clone(),
            get_mut: self.get_mut.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Id, Child, Get, GetMut> Copy for KeyedStatePath<Parent, Id, Child, Get, GetMut>
where
    Get: Copy,
    GetMut: Copy,
{
}

impl<Parent, Id, Child, Get, GetMut> fmt::Debug for KeyedStatePath<Parent, Id, Child, Get, GetMut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedStatePath").finish_non_exhaustive()
    }
}

/// Extracts borrowed keyed actions and embeds owned keyed actions.
///
/// The extractor uses a higher-ranked bound tying borrowed identifier and
/// child action fields to the parent action borrow. Embedding constructs one
/// owned [`KeyedAction`] and passes it to the captured embedder. The path adds
/// no `Clone` bound to the identifier or either action type.
///
/// # Examples
///
/// ```
/// use rca_core::{KeyedAction, KeyedActionPath};
///
/// enum Action {
///     Row(KeyedAction<u64, String>),
///     Parent,
/// }
///
/// let path = KeyedActionPath::new(
///     |action: &Action| match action {
///         Action::Row(action) => Some(action),
///         Action::Parent => None,
///     },
///     Action::Row,
/// );
/// let action = path.embed(7, String::from("tap"));
/// let (id, child) = path.extract(&action).unwrap();
/// assert_eq!((*id, child.as_str()), (7, "tap"));
/// ```
pub struct KeyedActionPath<Parent, Id, Child, Extract, Embed> {
    /// Borrows both the identifier and action from the same parent-owned payload.
    extract: Extract,
    /// Consumes the complete keyed output to construct a parent action.
    embed: Embed,
    /// Records domains without storing keys, action values or their auto-trait bounds.
    marker: PhantomData<fn(Parent) -> (Id, Child)>,
}

impl<Parent, Id, Child, Extract, Embed> KeyedActionPath<Parent, Id, Child, Extract, Embed>
where
    Extract: for<'a> Fn(&'a Parent) -> Option<&'a KeyedAction<Id, Child>>,
    Embed: Fn(KeyedAction<Id, Child>) -> Parent,
{
    /// Creates a keyed action path from extraction and embedding closures.
    #[must_use]
    pub const fn new(extract: Extract, embed: Embed) -> Self {
        Self {
            extract,
            embed,
            marker: PhantomData,
        }
    }

    /// Borrows the target identifier and child action when the path matches.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied extraction closure panics.
    #[must_use]
    pub fn extract<'a>(&self, parent: &'a Parent) -> Option<(&'a Id, &'a Child)> {
        (self.extract)(parent).map(|action| (action.id(), action.action()))
    }

    /// Embeds one owned identifier and child action into a parent action.
    ///
    /// # Panics
    ///
    /// This method only panics when the supplied embedding closure panics.
    #[must_use]
    pub fn embed(&self, id: Id, child: Child) -> Parent {
        (self.embed)(KeyedAction::new(id, child))
    }

    /// Copies only keyed output-routing metadata for an effect that escapes reduction.
    pub(crate) fn clone_embedder(&self) -> Embed
    where
        Embed: Clone,
    {
        self.embed.clone()
    }
}

impl<Parent, Id, Child, Extract, Embed> KeyedActionPath<Parent, Id, Child, Extract, Embed> {
    /// Transfers keyed output routing and drops the unused extraction captures.
    pub(crate) fn into_embedder(self) -> Embed {
        self.embed
    }
}

impl<Parent, Id, Child, Extract, Embed> Clone for KeyedActionPath<Parent, Id, Child, Extract, Embed>
where
    Extract: Clone,
    Embed: Clone,
{
    fn clone(&self) -> Self {
        Self {
            extract: self.extract.clone(),
            embed: self.embed.clone(),
            marker: PhantomData,
        }
    }
}

impl<Parent, Id, Child, Extract, Embed> Copy for KeyedActionPath<Parent, Id, Child, Extract, Embed>
where
    Extract: Copy,
    Embed: Copy,
{
}

impl<Parent, Id, Child, Extract, Embed> fmt::Debug
    for KeyedActionPath<Parent, Id, Child, Extract, Embed>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedActionPath").finish_non_exhaustive()
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{KeyedAction, KeyedActionPath};

    #[test]
    fn identity_paths_preserve_borrowed_storage_and_consumed_move_only_payloads() {
        struct Payload<'a>(&'a str, std::rc::Rc<()>);
        let label = String::from("local");
        let mut root = Payload(&label, std::rc::Rc::new(()));
        let path = super::identity_state_path();
        assert!(std::ptr::eq(path.get(&root), &raw const root));
        assert!(std::ptr::eq(path.get_mut(&mut root), &raw const root));
        path.get_mut(&mut root).0 = "changed";
        let identity = std::rc::Rc::as_ptr(&root.1);
        let action = super::identity_action_path().embed(root);
        let action_path = super::identity_action_path();
        assert!(std::ptr::eq(
            action_path.extract(&action).unwrap(),
            &raw const action
        ));
        assert_eq!(std::rc::Rc::as_ptr(&action.1), identity);
        assert_eq!(std::rc::Rc::strong_count(&action.1), 1);
        let mut rows: super::KeyedState<usize, _> = [(7, action)].into_iter().collect();
        let keyed_state = super::identity_keyed_state_path();
        assert!(std::ptr::eq(keyed_state.get(&rows), &raw const rows));
        assert!(std::ptr::eq(
            keyed_state.get_mut(&mut rows),
            &raw const rows
        ));
        keyed_state.get_mut(&mut rows).get_mut(&7).unwrap().0 = "keyed";
        assert_eq!(rows.get(&7).unwrap().0, "keyed");
        let payload = rows.remove(&7).unwrap();
        let id = Payload(&label, std::rc::Rc::new(()));
        let id_identity = std::rc::Rc::as_ptr(&id.1);
        let keyed_action_path = super::identity_keyed_action_path();
        let action = keyed_action_path.embed(id, payload);
        let (borrowed_id, borrowed_payload) = keyed_action_path.extract(&action).unwrap();
        assert!(std::ptr::eq(borrowed_id, action.id()));
        assert!(std::ptr::eq(borrowed_payload, action.action()));
        assert_eq!(std::rc::Rc::as_ptr(&borrowed_id.1), id_identity);
        assert_eq!(std::rc::Rc::as_ptr(&borrowed_payload.1), identity);
        assert_eq!(std::rc::Rc::strong_count(&borrowed_payload.1), 1);
    }

    #[test]
    fn three_level_stored_route_keeps_local_move_only_borrows() {
        struct MoveOnly<'a>(&'a str, std::rc::Rc<()>);
        let label = String::from("local");
        let path = super::some_path::<Result<Option<MoveOnly<'_>>, ()>>()
            .then(super::ok_path())
            .then(super::some_path())
            .lift_optional();
        let root = path.embed(MoveOnly(&label, std::rc::Rc::new(())));
        let Some(Some(Ok(Some(direct)))) = &root else {
            panic!("wrong embedding");
        };
        let borrowed = path.extract(&root).unwrap();
        assert!(std::ptr::eq(borrowed, direct));
        assert_eq!(borrowed.0, "local");
        assert_eq!(std::rc::Rc::strong_count(&borrowed.1), 1);
        assert!(path.extract(&None).is_none());
        assert!(path.extract(&Some(None)).is_none());
        assert!(path.extract(&Some(Some(Err(())))).is_none());
        assert!(path.extract(&Some(Some(Ok(None)))).is_none());
    }

    struct ScopedMoveOnly(usize);
    struct ScopedLeaf;

    impl crate::Reducer for ScopedLeaf {
        type State = usize;
        type Action = ScopedMoveOnly;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &ScopedMoveOnly,
            _: &mut crate::ReduceContext<'_>,
        ) -> crate::Effect<ScopedMoveOnly> {
            *state += action.0;
            crate::Effect::send(ScopedMoveOnly(action.0 + 1))
        }
    }

    #[test]
    fn three_level_owned_route_reduces_through_operational_scope() {
        use crate::Reducer;
        let path = super::some_path::<Result<Option<ScopedMoveOnly>, ()>>()
            .then(super::ok_path())
            .then(super::some_path())
            .into_action_path();
        let root = path.embed(ScopedMoveOnly(7));
        assert_eq!(path.extract(&root).unwrap().0, 7);
        let mut reducer = crate::scope(
            crate::StatePath::new(|state: &usize| state, |state: &mut usize| state),
            path,
            ScopedLeaf,
        );
        let mut state = 0;
        let mut dependencies = crate::DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = crate::store::scope::ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = crate::ReduceContext::new(
            &mut dependencies,
            crate::MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let effect = reducer.reduce(&mut state, &root, &mut context);
        assert_eq!(state, 7);
        assert!(matches!(
            effect.program,
            crate::effect::Program::Send(Some(Ok(Some(ScopedMoveOnly(8)))))
        ));
        let _ = reducer.reduce(&mut state, &None, &mut context);
        let _ = reducer.reduce(&mut state, &Some(Err(())), &mut context);
        let _ = reducer.reduce(&mut state, &Some(Ok(None)), &mut context);
        assert_eq!(state, 7);
    }

    #[test]
    fn lifted_standard_route_reduces_through_operational_scope() {
        use crate::Reducer;
        let path = super::some_path::<ScopedMoveOnly>()
            .lift_optional()
            .into_action_path();
        let root = path.embed(ScopedMoveOnly(5));
        let mut reducer = crate::scope(
            crate::StatePath::new(|state: &usize| state, |state: &mut usize| state),
            path,
            ScopedLeaf,
        );
        let mut state = 0;
        let mut dependencies = crate::DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = crate::store::scope::ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = crate::ReduceContext::new(
            &mut dependencies,
            crate::MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let effect = reducer.reduce(&mut state, &root, &mut context);
        assert!(matches!(
            effect.program,
            crate::effect::Program::Send(Some(Some(ScopedMoveOnly(6))))
        ));
        let _ = reducer.reduce(&mut state, &None, &mut context);
        let _ = reducer.reduce(&mut state, &Some(None), &mut context);
        assert_eq!(state, 5);
    }

    #[test]
    fn owned_conversion_clones_only_embedding_captures() {
        struct NonCloneCapture(Arc<AtomicUsize>);
        let extraction_calls = Arc::new(AtomicUsize::new(0));
        let extraction = NonCloneCapture(Arc::clone(&extraction_calls));
        let embedding_clones = Arc::new(AtomicUsize::new(0));
        let embedding = CloneProbe {
            clones: Arc::clone(&embedding_clones),
        };
        let path = super::ActionPath::new(
            move |root: &Option<ScopedMoveOnly>| {
                // Capture the complete non-Clone value, not just its Arc field.
                fn record(capture: &NonCloneCapture) {
                    capture.0.fetch_add(1, Ordering::Relaxed);
                }
                record(&extraction);
                root.as_ref()
            },
            move |value| {
                embedding.touch();
                Some(value)
            },
        )
        .lift_optional()
        .into_action_path();
        assert_eq!(embedding_clones.load(Ordering::Relaxed), 1);
        let root = path.embed(ScopedMoveOnly(9));
        assert_eq!(path.extract(&root).unwrap().0, 9);
        assert_eq!(extraction_calls.load(Ordering::Relaxed), 1);
        let embed = path.clone_embedder();
        assert_eq!(embedding_clones.load(Ordering::Relaxed), 2);
        assert!(matches!(
            embed(ScopedMoveOnly(3)),
            Some(Some(ScopedMoveOnly(3)))
        ));
        assert_eq!(extraction_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn recursive_carriers_are_copy_without_copy_or_clone_payloads() {
        let path = super::some_path::<Result<Option<ScopedMoveOnly>, ()>>()
            .then(super::ok_path())
            .then(super::some_path())
            .lift_optional();
        let copied = path;
        let root = path.embed(ScopedMoveOnly(4));
        assert_eq!(copied.extract(&root).unwrap().0, 4);
        let restored = copied
            .then(super::ActionPath::new(
                |root: &ScopedMoveOnly| Some(root),
                |value| value,
            ))
            .embed(ScopedMoveOnly(6));
        assert!(matches!(restored, Some(Some(Ok(Some(ScopedMoveOnly(6)))))));
    }

    #[test]
    fn nested_stored_paths_borrow_non_clone_local_values_and_embed_in_order() {
        struct Payload<'a>(&'a str);
        enum Inner<'a> {
            Value(Payload<'a>),
            Other,
        }
        enum Outer<'a> {
            Inner(Inner<'a>),
            Other,
        }
        let label = String::from("local");
        let outer = super::ActionPath::new(
            |root: &Outer<'_>| match root {
                Outer::Inner(value) => Some(value),
                Outer::Other => None,
            },
            Outer::Inner,
        );
        let inner = super::ActionPath::new(
            |root: &Inner<'_>| match root {
                Inner::Value(value) => Some(value),
                Inner::Other => None,
            },
            Inner::Value,
        );
        let path = outer.then(inner);
        let root = path.embed(Payload(&label));
        let Outer::Inner(Inner::Value(direct)) = &root else {
            panic!("wrong embedding");
        };
        assert!(std::ptr::eq(path.extract(&root).unwrap(), direct));
        assert_eq!(path.extract(&root).unwrap().0, "local");
        assert!(path.extract(&Outer::Other).is_none());
        assert!(path.extract(&Outer::Inner(Inner::Other)).is_none());
    }

    #[test]
    fn optional_layers_distinguish_missing_root_matched_none_and_some() {
        let path = super::some_path::<Option<String>>().lift_optional();
        let absent = None;
        let wrong = Some(None);
        let matched_none = Some(Some(None));
        let matched_some = Some(Some(Some(String::from("value"))));
        assert!(path.extract(&absent).is_none());
        assert!(path.extract(&wrong).is_none());
        assert_eq!(path.extract(&matched_none), Some(&None));
        assert_eq!(
            path.extract(&matched_some),
            Some(&Some(String::from("value")))
        );
        assert_eq!(path.embed(None), matched_none);
        assert_eq!(path.embed(Some(String::from("value"))), matched_some);
    }

    #[test]
    fn standard_result_and_unit_paths_match_only_the_selected_case() {
        struct MoveOnly(u8);
        let ok = super::ok_path::<MoveOnly, MoveOnly>();
        let err = super::err_path::<MoveOnly, MoveOnly>();
        let success = ok.embed(MoveOnly(7));
        let failure = err.embed(MoveOnly(9));
        assert_eq!(ok.extract(&success).unwrap().0, 7);
        assert!(ok.extract(&failure).is_none());
        assert_eq!(err.extract(&failure).unwrap().0, 9);
        assert!(err.extract(&success).is_none());
        let none = super::none_path::<MoveOnly>();
        assert_eq!(none.extract(&None), Some(&()));
        assert!(none.extract(&Some(MoveOnly(1))).is_none());
        assert!(none.embed(()).is_none());
        let unit = super::unit_path();
        assert_eq!(unit.extract(&()), Some(&()));
        let () = unit.embed(());
    }

    #[test]
    fn computed_paths_preserve_validation_errors_and_declared_text_normalisation() {
        #[derive(Debug, PartialEq)]
        enum ConversionError {
            Parse(std::num::ParseIntError),
            NonPositive(u32),
        }
        struct Positive(u32);
        let text = super::ComputedPath::new(
            |root: &String| root.parse::<u32>().map_err(ConversionError::Parse),
            |value: u32| value.to_string(),
        );
        let raw = super::ComputedPath::new(
            |root: &u32| {
                if *root == 0 {
                    Err(ConversionError::NonPositive(*root))
                } else {
                    Ok(Positive(*root))
                }
            },
            |value: Positive| value.0,
        );
        assert_eq!(raw.extract(&7).unwrap().0, 7);
        assert!(matches!(
            raw.extract(&0),
            Err(ConversionError::NonPositive(0))
        ));
        assert_eq!(raw.embed(Positive(7)), 7);
        assert_eq!(text.embed(text.extract(&String::from("007")).unwrap()), "7");
        let path = text.then(raw);
        assert_eq!(path.extract(&String::from("007")).unwrap().0, 7);
        assert!(matches!(
            path.extract(&String::from("bad")),
            Err(ConversionError::Parse(error)) if *error.kind() == std::num::IntErrorKind::InvalidDigit
        ));
        assert!(matches!(
            path.extract(&String::from("000")),
            Err(ConversionError::NonPositive(0))
        ));
        assert_eq!(path.embed(Positive(7)), "7");
    }

    #[test]
    fn computed_composition_short_circuits_outer_error_with_local_closures() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let suffix = String::from("local");
        let outer = super::ComputedPath::new(
            |root: &bool| if *root { Ok(()) } else { Err("outer") },
            |()| true,
        );
        let inner = super::ComputedPath::new(
            |(): &()| {
                calls.set(calls.get() + 1);
                Ok(suffix.as_str())
            },
            |_: &str| (),
        );
        let path = outer.then(inner);
        assert_eq!(path.extract(&false), Err("outer"));
        assert_eq!(calls.get(), 0);
        assert_eq!(path.extract(&true), Ok("local"));
        assert_eq!(calls.get(), 1);
        assert!(path.embed("different"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn constant_path_synthesises_fresh_move_only_values_only_on_extraction() {
        use std::cell::Cell;
        struct MoveOnly<'a> {
            serial: usize,
            drops: &'a Cell<usize>,
        }
        impl Drop for MoveOnly<'_> {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let calls = Cell::new(0);
        let drops = Cell::new(0);
        let path = super::constant_path(|| {
            calls.set(calls.get() + 1);
            MoveOnly {
                serial: calls.get(),
                drops: &drops,
            }
        });
        let first = path.extract(&()).unwrap();
        let second = path.extract(&()).unwrap();
        assert_eq!((first.serial, second.serial), (1, 2));
        assert_eq!(calls.get(), 2);
        let () = path.embed(first);
        assert_eq!(calls.get(), 2);
        assert_eq!(drops.get(), 1);
        drop(second);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn stored_path_lifting_keeps_caller_local_captures() {
        use std::cell::Cell;
        struct MoveOnly(u8);
        let allowed = Cell::new(true);
        let path = super::ActionPath::new(
            |root: &MoveOnly| allowed.get().then_some(root),
            |value: MoveOnly| value,
        )
        .lift_optional();
        let root = path.embed(MoveOnly(4));
        assert_eq!(path.extract(&root).unwrap().0, 4);
        allowed.set(false);
        assert!(path.extract(&root).is_none());
        assert!(path.extract(&None).is_none());
    }

    #[test]
    fn standard_descriptor_preserves_existing_scoped_effect_embedding_bounds() {
        struct MoveOnly;
        struct Leaf;
        impl crate::Reducer for Leaf {
            type State = ();
            type Action = MoveOnly;
            fn reduce(
                &mut self,
                (): &mut (),
                _: &MoveOnly,
                _: &mut crate::ReduceContext<'_>,
            ) -> crate::Effect<MoveOnly> {
                crate::Effect::none()
            }
        }
        fn assert_reducer(_: impl crate::Reducer<State = (), Action = Option<MoveOnly>>) {}
        let reducer = crate::scope(
            crate::StatePath::new(|state: &()| state, |state: &mut ()| state),
            super::some_path::<MoveOnly>(),
            Leaf,
        );
        assert_reducer(reducer);
    }

    #[derive(Debug)]
    struct CloneProbe {
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl CloneProbe {
        fn touch(&self) {
            let _ = self.clones.load(Ordering::Relaxed);
        }
    }

    struct MoveOnlyId;
    struct MoveOnlyAction;
    struct Parent(KeyedAction<MoveOnlyId, MoveOnlyAction>);

    #[test]
    fn keyed_embedder_clone_clones_only_captured_embedder_state() {
        let clones = Arc::new(AtomicUsize::new(0));
        let probe = CloneProbe {
            clones: Arc::clone(&clones),
        };
        let path = KeyedActionPath::new(
            |parent: &Parent| Some(&parent.0),
            move |action| {
                probe.touch();
                Parent(action)
            },
        );

        let embed = path.clone_embedder();
        let _ = embed(KeyedAction::new(MoveOnlyId, MoveOnlyAction));

        assert_eq!(clones.load(Ordering::Relaxed), 1);
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::keyed_path_operations_do_not_allocate;

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{KeyedAction, KeyedActionPath, KeyedState};

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn keyed_path_operations_do_not_allocate() {
        struct State {
            rows: KeyedState<u64, u8>,
        }

        enum Action {
            Row(KeyedAction<u64, u8>),
            Parent,
        }

        if crate::allocation_test::run_in_child(
            "composition::path::tests::keyed_path_operations_do_not_allocate",
        ) {
            return;
        }

        let state_path = super::KeyedStatePath::new(
            |state: &State| &state.rows,
            |state: &mut State| &mut state.rows,
        );
        let action_path = KeyedActionPath::new(
            |action: &Action| match action {
                Action::Row(action) => Some(action),
                Action::Parent => None,
            },
            Action::Row,
        );
        let mut state = State {
            rows: KeyedState::new(),
        };
        let action = Action::Row(KeyedAction::new(7, 3));

        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        black_box(state_path.get(&state));
        black_box(state_path.get_mut(&mut state));
        black_box(action_path.extract(&action));
        black_box(action_path.embed(7, 4));
        let after = dhat::HeapStats::get();

        assert_eq!(after.total_blocks, before.total_blocks);
        assert_eq!(after.total_bytes, before.total_bytes);
        drop(profiler);
        let _ = Action::Parent;
    }
}
