//! Borrowed projections of enum cases with separately stored fields.

use core::marker::PhantomData;

/// Describes one enum case's owned payload and borrowed field aggregate.
///
/// Derives use declaration-order tuples for both tuple and named variants.
/// Each borrowed tuple element refers directly to its corresponding root field.
/// Unit cases use `()` and still distinguish matching from non-matching cases.
pub trait AggregateCase {
    /// The original enum, including its generic parameters and lifetimes.
    type Root;
    /// Owned fields consumed when embedding this case.
    type Payload;
    /// Shared references to the separately stored fields.
    type Borrowed<'a>
    where
        Self: 'a;

    /// Borrows the fields when the root matches this case.
    fn extract<'a>(root: &'a Self::Root) -> Option<Self::Borrowed<'a>>
    where
        Self: 'a;

    /// Constructs the original enum case from owned fields.
    fn embed(payload: Self::Payload) -> Self::Root;
}

/// Adds disjoint mutable field borrowing to a state case.
pub trait MutableAggregateCase: AggregateCase {
    /// Mutable references tied to the original root borrow.
    type BorrowedMut<'a>
    where
        Self: 'a;

    /// Borrows the separately stored fields mutably when the case matches.
    fn get_mut<'a>(root: &'a mut Self::Root) -> Option<Self::BorrowedMut<'a>>
    where
        Self: 'a;
}

/// A typed action case returning an aggregate of field borrows.
///
/// Unlike a one-field [`super::ActionPath`], this path does not return a
/// reference to an owned tuple: no such tuple exists in the enum's storage.
/// It imposes no cloning or transport bounds on fields.
pub struct AggregateActionPath<Case>(
    /// Names the projection without owning a case or inheriting its transport bounds.
    PhantomData<fn() -> Case>,
);

impl<Case> core::fmt::Debug for AggregateActionPath<Case> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AggregateActionPath")
    }
}

impl<Case> Copy for AggregateActionPath<Case> {}

impl<Case> Clone for AggregateActionPath<Case> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Case: AggregateCase> Default for AggregateActionPath<Case> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Case: AggregateCase> AggregateActionPath<Case> {
    /// Creates the zero-sized case path.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// Borrows the selected case's fields, or returns `None` for another case.
    pub fn extract<'a>(&self, root: &'a Case::Root) -> Option<Case::Borrowed<'a>>
    where
        Case: 'a,
    {
        Case::extract(root)
    }

    /// Embeds owned fields into the selected enum case.
    pub fn embed(&self, payload: Case::Payload) -> Case::Root {
        Case::embed(payload)
    }
}

/// A typed state case with shared and disjoint mutable field projections.
pub struct AggregateStatePath<Case>(
    /// Names the projection without owning state or imposing bounds on its fields.
    PhantomData<fn() -> Case>,
);

impl<Case> core::fmt::Debug for AggregateStatePath<Case> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AggregateStatePath")
    }
}

impl<Case> Copy for AggregateStatePath<Case> {}

impl<Case> Clone for AggregateStatePath<Case> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Case: MutableAggregateCase> Default for AggregateStatePath<Case> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Case: MutableAggregateCase> AggregateStatePath<Case> {
    /// Creates the zero-sized case path.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// Borrows the selected case's fields, or returns `None` for another case.
    pub fn get<'a>(&self, root: &'a Case::Root) -> Option<Case::Borrowed<'a>>
    where
        Case: 'a,
    {
        Case::extract(root)
    }

    /// Mutably borrows disjoint fields in the original root.
    pub fn get_mut<'a>(&self, root: &'a mut Case::Root) -> Option<Case::BorrowedMut<'a>>
    where
        Case: 'a,
    {
        Case::get_mut(root)
    }

    /// Embeds owned fields into the selected enum case.
    pub fn embed(&self, payload: Case::Payload) -> Case::Root {
        Case::embed(payload)
    }
}
