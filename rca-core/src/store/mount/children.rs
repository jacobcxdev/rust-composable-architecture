//! Construction-only child declarations sharing immutable parent route metadata.

use super::nested::{NestedOwner, RecipeOwner};
use super::{
    Arc, Binding, BorrowPath, Carriers, Descriptor, EntryMount, Feature, Hash, MountConstruction,
    MountDeclaration, MountIdentity, MountSite, OptionalMount, OptionalPath, ScopeToken, Shared,
    attach_descriptor, fmt,
};
use crate::store::mounted::erased_path::SharedPath;
use crate::store::mounted::path::ComposedPath;

/// Declares children for one installed parent incarnation.
///
/// Before a recipe starts, its parent must still be selectable. If selection is
/// absent, discovery stays pending until a later structural transaction can
/// select the parent. A recipe that returns normally is not repeated for the
/// same parent incarnation.
pub struct MountChildren<'a, R, A, S, X> {
    /// Shares the installed parent's route across sibling declarations.
    path: &'a SharedPath<R, S>,
    /// Exposes weak acquisition and handler identity for this parent.
    site: &'a MountSite<R, S, X, A>,
    /// Attaches each discovered declaration to this live parent incarnation.
    parent: ScopeToken,
    /// Locates this parent in the actor's preorder declaration roster.
    depth: usize,
    /// Borrows root admission metadata without retaining the Store owner.
    binding: &'a Binding<R, A>,
    /// Receives declarations in recipe order for subsequent structural traversal.
    discovered: &'a mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
}

impl<R, A, S, X> fmt::Debug for MountChildren<'_, R, A, S, X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountChildren")
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl<R: Send + 'static, A: Send + 'static, S: 'static, X: 'static> MountChildren<'_, R, A, S, X> {
    /// Borrows the actual weak site of this installed parent declaration.
    #[must_use]
    pub fn site(&self) -> &MountSite<R, S, X, A> {
        self.site
    }

    /// Declares a send-only ordinary input for this installed incarnation.
    /// The actor owns the mapper; old ports never retarget replacement children.
    pub fn command<C: Send + 'static>(
        &mut self,
        mapper: impl FnMut(C) -> X + Send + 'static,
    ) -> crate::CommandSite<R, C>
    where
        X: Send,
    {
        let (owner, site) = super::commands::attach(
            self.site,
            self.binding,
            mapper,
            crate::store::mounted::InputClass::Ordinary,
        );
        self.push(owner);
        site
    }

    /// Declares an independently typed control input on the same incarnation.
    /// Only this input bypasses the input gate; generated tasks/effects do not.
    pub fn control<C: Send + 'static>(
        &mut self,
        mapper: impl FnMut(C) -> X + Send + 'static,
    ) -> crate::CommandSite<R, C>
    where
        X: Send,
    {
        let (owner, site) = super::commands::attach(
            self.site,
            self.binding,
            mapper,
            crate::store::mounted::InputClass::Control,
        );
        self.push(owner);
        site
    }

    /// Declares this incarnation's owned contribution to a typed fold.
    ///
    /// # Panics
    /// Panics if this incarnation already declares the same channel.
    pub fn contribute<K: crate::FoldKey>(
        &mut self,
        fold: &crate::Fold<R, K>,
        value: impl Fn(&S) -> K::Value + Send + 'static,
    ) where
        K::Value: Send,
    {
        self.push(fold.attach(self.site, Some(Box::new(value)), None));
    }

    /// Declares a contribution and transforms only this incarnation's subtree.
    ///
    /// # Panics
    /// Panics if this incarnation already declares the same channel.
    pub fn contribute_transformed<K: crate::FoldKey>(
        &mut self,
        fold: &crate::Fold<R, K>,
        value: impl Fn(&S) -> K::Value + Send + 'static,
        transform: impl Fn(K::Value) -> K::Value + Send + 'static,
    ) where
        K::Value: Send,
    {
        self.push(fold.attach(self.site, Some(Box::new(value)), Some(Box::new(transform))));
    }

    /// Transforms this incarnation's subtree without adding a contribution.
    ///
    /// # Panics
    /// Panics if this incarnation already declares the same channel.
    pub fn transform_fold<K: crate::FoldKey>(
        &mut self,
        fold: &crate::Fold<R, K>,
        transform: impl Fn(K::Value) -> K::Value + Send + 'static,
    ) where
        K::Value: Send,
    {
        self.push(fold.attach(self.site, None, Some(Box::new(transform))));
    }

    /// Declares every keyed entry with a fresh, owned Feature and callbacks.
    ///
    /// Reordering retains each entry's Feature, caches and weak sites. A new
    /// entry stamp or explicit scope leave creates a fresh construction; a
    /// configuration-only change retains the Feature but expires old authority.
    /// Use `keyed_with_children` to declare descendants or retain entry sites.
    pub fn keyed<F, K, ChildState>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> EntryMount<F> + Send + 'static,
    ) where
        F: Feature<State = ChildState> + Send + 'static,
        ChildState: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let owner = keyed(
            self.path.clone(),
            read,
            write,
            self.depth + 1,
            move |key, state, _| {
                (
                    make(key, state),
                    None::<fn(&ChildState, &mut MountChildren<'_, R, A, ChildState, F::Action>)>,
                )
            },
        );
        self.push(owner);
    }

    /// Declares keyed entries with separate incarnation-owned child recipes.
    ///
    /// All departing subtrees of this collection are drained before any new
    /// `make` call. New entries are constructed in keyed order, then the combined
    /// retained/new roster is visited depth-first. Retained nested collections
    /// reconcile during that traversal: this is not a global retirement pass.
    /// Reorder and configuration changes preserve the existing Feature/recipe;
    /// replacement or explicit scope leave creates both afresh. Clone an ID or
    /// otherwise own values that the returned recipe retains from `make`.
    ///
    /// # Examples
    ///
    /// Own the ID and row-derived values retained by the returned recipe.
    ///
    /// ```
    /// use rca_core::{EntryMount, KeyedState, MountChildren};
    /// use std::cell::Cell;
    /// # use rca_core::{Feature, FeatureContext};
    /// # struct RowFeature(Cell<usize>);
    /// # impl Feature for RowFeature {
    /// #     type State = usize;
    /// #     type Action = ();
    /// #     fn update(&mut self, state: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) { self.0.set(*state); }
    /// # }
    /// # type Rows = KeyedState<String, usize>;
    /// # fn declare(children: &mut MountChildren<'_, Rows, (), Rows, ()>) {
    /// children.keyed_with_children(|rows| rows, |rows| rows, |id, row| {
    ///     let id = id.clone();
    ///     let seed = *row;
    ///     (EntryMount::new(RowFeature(Cell::new(seed))), move |_, children| {
    ///         let _ = (&id, seed, children.site());
    ///     })
    /// });
    /// # }
    /// ```
    pub fn keyed_with_children<F, K, G, ChildState>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> (EntryMount<F>, G) + Send + 'static,
    ) where
        F: Feature<State = ChildState> + Send + 'static,
        ChildState: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
        G: FnMut(&ChildState, &mut MountChildren<'_, R, A, ChildState, F::Action>) + Send + 'static,
    {
        let owner = keyed(
            self.path.clone(),
            read,
            write,
            self.depth + 1,
            move |key, state, _| {
                let (entry, recipe) = make(key, state);
                (entry, Some(recipe))
            },
        );
        self.push(owner);
    }

    /// Declares keyed children with checked, incarnation-bound owning removal sites.
    pub fn keyed_removable<F, K, G>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, F::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, F::State>,
        make: impl FnMut(&K, &F::State) -> (EntryMount<F>, G) + Send + 'static,
    ) where
        S: Send,
        X: Send,
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
        G: FnMut(
                &F::State,
                &mut MountChildren<'_, R, A, F::State, F::Action>,
                crate::RemovalSite<S, F::State, X>,
            ) + Send
            + 'static,
    {
        let owner = keyed_removable(
            self.path.clone(),
            self.site.handler_registration(),
            read,
            write,
            self.depth + 1,
            make,
        );
        self.push(owner);
    }

    /// Declares native stack entries with checked pop-last removal sites.
    /// A live non-last target returns `InvalidTarget` without removal callbacks.
    pub fn stack_removable<F, G>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::StackState<F::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::StackState<F::State>,
        make: impl FnMut(&crate::StackId, &F::State) -> (EntryMount<F>, G) + Send + 'static,
    ) where
        S: Send,
        X: Send,
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        G: FnMut(
                &F::State,
                &mut MountChildren<'_, R, A, F::State, F::Action>,
                crate::RemovalSite<S, F::State, X>,
            ) + Send
            + 'static,
    {
        let owner = stack_removable(
            self.path.clone(),
            self.site.handler_registration(),
            read,
            write,
            self.depth + 1,
            make,
        );
        self.push(owner);
    }

    /// Declares output handlers owned by this parent incarnation.
    pub fn output<O: Send + 'static>(
        &mut self,
        source: &MountSite<R, impl Send + 'static, impl Send + 'static, A>,
        configure: impl FnOnce(&mut super::super::output::OutputHandlers<R, A, O>),
    ) -> super::super::output::Output<O> {
        let (owner, output) =
            super::super::output::attach_owner(source.output_registration(), configure);
        self.push(owner);
        output
    }

    /// Declares an optional child beneath this parent incarnation.
    ///
    /// The returned removal site owns only weak declaration metadata. Its two
    /// field selectors are shared by mounting and checked ownership extraction.
    /// `read` and `write` must select the same owning field and agree with
    /// `identity`. Selectors borrow State without mutating it or replacing the
    /// child. External side effects and panics remain permitted.
    #[expect(
        clippy::type_complexity,
        reason = "Returns the declaration and typed acquisition or removal authority."
    )]
    pub fn optional_removable<F, K>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s Option<F::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut Option<F::State>,
        mut identity: impl FnMut(&S) -> Option<MountIdentity<K>> + Send + 'static,
        make: impl FnMut(&F::State) -> F + Send + 'static,
    ) -> (
        MountSite<R, F::State, F::Action, A>,
        crate::RemovalSite<S, F::State, X>,
    )
    where
        S: Send,
        X: Send,
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let parent_path = self.path.clone();
        let descriptor = Descriptor {
            path: ComposedPath::new(
                self.path.clone(),
                crate::store::mounted::path::OwnedOptionalPath { read, write },
            ),
            identity: Box::new(move |root, live| {
                let parent = parent_path.read_structural(root, live)?;
                if !live() {
                    return None;
                }
                identity(parent)
            }),
            make: Box::new(make),
            initial: None,
            reactions: Vec::new(),
        };
        let (owner, site) = attach_descriptor(descriptor);
        let removal = crate::store::removal::optional(
            self.site.handler_registration(),
            site.output_registration(),
            std::sync::Weak::clone(&self.binding.ingress),
            write,
        );
        self.push(owner);
        (site, removal)
    }

    /// Declares an optional child beneath this parent incarnation.
    pub fn optional<F, K>(
        &mut self,
        descriptor: OptionalMount<S, F, K>,
    ) -> MountSite<R, F::State, F::Action, A>
    where
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let descriptor = self.compose(descriptor);
        let (owner, site) = attach_descriptor(descriptor);
        self.push(owner);
        site
    }

    /// Declares an optional child with its own incarnation-bound recipe.
    pub fn optional_with_children<F, K>(
        &mut self,
        descriptor: OptionalMount<S, F, K>,
        recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>)
        + Send
        + 'static,
    ) -> MountSite<R, F::State, F::Action, A>
    where
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let descriptor = self.compose(descriptor);
        let depth = self.depth + 1;
        let (owner, site) = attach_recipe(descriptor, recipe, depth);
        self.push(owner);
        site
    }

    /// Prefixes an optional declaration with this parent's shared route.
    #[expect(
        clippy::type_complexity,
        reason = "Preserves the composed parent and child path types."
    )]
    fn compose<F, K>(
        &self,
        descriptor: OptionalMount<S, F, K>,
    ) -> Descriptor<R, F, K, ComposedPath<SharedPath<R, S>, OptionalPath<S, F::State>>>
    where
        F: Feature,
        K: 'static,
    {
        let OptionalMount {
            path,
            mut identity,
            make,
            initial,
            reactions,
        } = descriptor;
        let parent_path = self.path.clone();
        Descriptor {
            path: ComposedPath::new(self.path.clone(), path),
            identity: Box::new(move |root, live| {
                let parent = parent_path.read_structural(root, live)?;
                if !live() {
                    return None;
                }
                identity(parent, live)
            }),
            make,
            initial,
            reactions,
        }
    }

    /// Binds a declaration before exposing it to later actor traversal.
    fn push(&mut self, inner: impl MountDeclaration<R, A> + Send + 'static) {
        let owner = NestedOwner {
            inner,
            parent: self.parent,
            depth: self.depth + 1,
        };
        let ingress = self
            .binding
            .ingress
            .upgrade()
            .expect("reconciling actor has ingress");
        let lifecycle = self
            .binding
            .lifecycle
            .upgrade()
            .expect("reconciling actor has lifecycle");
        owner.bind(&ingress, &lifecycle);
        self.discovered.push(Box::new(owner));
    }
}

/// Constructs entry-owned Features and routes from the collection's current insertion stamps.
///
/// Key cloning and user construction are distinct callback boundaries. The
/// collection checks root admission before invoking this constructor; each
/// callback is followed by another check before the next stage starts.
pub(in crate::store) fn keyed<R, A, S, F, K, P, G>(
    path: P,
    read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, F::State>,
    write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, F::State>,
    depth: usize,
    mut make: impl FnMut(
        &Arc<K>,
        &F::State,
        &crate::composition::EntryStamp,
    ) -> (EntryMount<F>, Option<G>)
    + Send
    + 'static,
) -> impl MountDeclaration<R, A> + Send
where
    R: Send + 'static,
    A: Send + 'static,
    S: 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = S> + Send + Sync + 'static,
    G: FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + Send + 'static,
{
    let entry_path = path.clone();
    super::collection::Collection::new(
        path,
        read,
        depth,
        move |key: &K,
              state: &F::State,
              stamp: crate::composition::EntryStamp,
              parent,
              depth,
              binding: &Binding<R, A>|
              -> Option<Box<<Shared as Carriers<R, A>>::Mount>> {
            let live = || {
                binding
                    .lifecycle
                    .upgrade()
                    .is_some_and(|lifecycle| !lifecycle.is_closed())
            };
            let key = Arc::new(key.clone());
            if !live() {
                return None;
            }
            let (
                EntryMount(MountConstruction {
                    feature,
                    initial,
                    reactions,
                    mut configuration,
                }),
                recipe,
            ) = make(&key, state, &stamp);
            if !live() {
                return None;
            }
            let path = ComposedPath::new(
                entry_path.clone(),
                super::super::mounted::path::KeyedPath::new(read, write, key),
            );
            let identity_path = path.clone();
            let identity_stamp = stamp.clone();
            let mut feature = Some(feature);
            let descriptor = Descriptor {
                path,
                identity: Box::new(move |root, live| {
                    let state = identity_path.read_structural(root, live)?;
                    if !live() {
                        return None;
                    }
                    let configuration = configuration.as_mut().map_or(0, |select| select(state));
                    if !live() {
                        return None;
                    }
                    Some(MountIdentity {
                        key: identity_stamp.clone(),
                        replacement: 0,
                        configuration,
                    })
                }),
                make: Box::new(move |_| {
                    feature
                        .take()
                        .expect("collection replaces retired entry construction")
                }),
                initial,
                reactions,
            };
            Some(if let Some(recipe) = recipe {
                let (inner, _) = attach_recipe(descriptor, recipe, depth);
                Box::new(super::collection::bind_entry(
                    inner, stamp, parent, depth, binding,
                ))
            } else {
                let (inner, _) = attach_descriptor(descriptor);
                Box::new(super::collection::bind_entry(
                    inner, stamp, parent, depth, binding,
                ))
            })
        },
    )
}

/// Adds weak owning-removal sites to each keyed entry's one-shot child recipe.
pub(in crate::store) fn keyed_removable<R, A, S, X, F, K, P, G>(
    path: P,
    parent: std::sync::Weak<dyn crate::store::output::HandlerRegistration<R, S, X, A>>,
    read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, F::State>,
    write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, F::State>,
    depth: usize,
    mut make: impl FnMut(&K, &F::State) -> (EntryMount<F>, G) + Send + 'static,
) -> impl MountDeclaration<R, A> + Send
where
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    X: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = S> + Send + Sync + 'static,
    G: FnMut(
            &F::State,
            &mut MountChildren<'_, R, A, F::State, F::Action>,
            crate::RemovalSite<S, F::State, X>,
        ) + Send
        + 'static,
{
    keyed(path, read, write, depth, move |key, state, stamp| {
        let (entry, mut recipe) = make(key, state);
        let (key, stamp, parent) = (
            Arc::clone(key),
            stamp.clone(),
            std::sync::Weak::clone(&parent),
        );
        (
            entry,
            Some(
                move |state: &F::State,
                      children: &mut MountChildren<'_, R, A, F::State, F::Action>| {
                    let removal = crate::store::removal::keyed(
                        std::sync::Weak::clone(&parent),
                        children.site.output_registration(),
                        std::sync::Weak::clone(&children.binding.ingress),
                        write,
                        Arc::clone(&key),
                        stamp.clone(),
                    );
                    recipe(state, children, removal);
                },
            ),
        )
    })
}

/// Adapts stack entries to the collection protocol while retaining pop-last removal.
pub(in crate::store) fn stack_removable<R, A, S, X, F, P, G>(
    path: P,
    parent: std::sync::Weak<dyn crate::store::output::HandlerRegistration<R, S, X, A>>,
    read: for<'s> fn(&'s S) -> &'s crate::StackState<F::State>,
    write: for<'s> fn(&'s mut S) -> &'s mut crate::StackState<F::State>,
    depth: usize,
    mut make: impl FnMut(&crate::StackId, &F::State) -> (EntryMount<F>, G) + Send + 'static,
) -> impl MountDeclaration<R, A> + Send
where
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    X: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    P: BorrowPath<R, State = S> + Send + Sync + 'static,
    G: FnMut(
            &F::State,
            &mut MountChildren<'_, R, A, F::State, F::Action>,
            crate::RemovalSite<S, F::State, X>,
        ) + Send
        + 'static,
{
    let path = ComposedPath::new(
        path,
        super::super::mounted::path::StackContainerPath { read, write },
    );
    keyed(
        path,
        |entries| entries,
        |entries| entries,
        depth,
        move |key, state, stamp| {
            let (entry, mut recipe) = make(key, state);
            let (key, stamp, parent) = (
                Arc::clone(key),
                stamp.clone(),
                std::sync::Weak::clone(&parent),
            );
            (
                entry,
                Some(
                    move |state: &F::State,
                          children: &mut MountChildren<'_, R, A, F::State, F::Action>| {
                        let removal = crate::store::removal::stack(
                            std::sync::Weak::clone(&parent),
                            children.site.output_registration(),
                            std::sync::Weak::clone(&children.binding.ingress),
                            write,
                            Arc::clone(&key),
                            stamp.clone(),
                        );
                        recipe(state, children, removal);
                    },
                ),
            )
        },
    )
}

/// Attaches a root-relative optional declaration and its deferred child recipe.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration and typed acquisition or removal authority."
)]
pub(in crate::store) fn attach<R, A, F, K>(
    descriptor: OptionalMount<R, F, K>,
    recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + Send + 'static,
) -> (
    Box<dyn MountDeclaration<R, A> + Send>,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let OptionalMount {
        path,
        identity,
        make,
        initial,
        reactions,
    } = descriptor;
    let (owner, site) = attach_recipe(
        Descriptor {
            path,
            identity,
            make,
            initial,
            reactions,
        },
        recipe,
        0,
    );
    (Box::new(owner), site)
}

/// Runs a recipe once after its parent can be selected for construction.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration and typed acquisition or removal authority."
)]
fn attach_recipe<R, A, F, K, P>(
    descriptor: Descriptor<R, F, K, P>,
    mut recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>)
    + Send
    + 'static,
    depth: usize,
) -> (
    impl MountDeclaration<R, A> + Send,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
{
    let path = descriptor.path.clone();
    let (inner, site) = attach_descriptor(descriptor);
    let parent_site = site.clone();
    let owner = RecipeOwner::new(
        inner,
        move |root: &R,
              parent,
              _inner_depth,
              binding: &Binding<R, A>,
              discovered: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>| {
            let live = || {
                binding
                    .lifecycle
                    .upgrade()
                    .is_some_and(|lifecycle| !lifecycle.is_closed())
            };
            let state = path.read_structural(root, &live);
            if !live() {
                return false;
            }
            // Construction can hide the path without ending this incarnation.
            // Leave discovery pending until a later reconciliation can select it.
            let Some(state) = state else {
                return false;
            };
            // One opt-in metadata allocation per parent incarnation, including an
            // empty recipe. All sibling routes produced below share this owner.
            let path = SharedPath::new(path.clone());
            recipe(
                state,
                &mut MountChildren {
                    path: &path,
                    site: &parent_site,
                    parent,
                    depth,
                    binding,
                    discovered,
                },
            );
            true
        },
    );
    (owner, site)
}
