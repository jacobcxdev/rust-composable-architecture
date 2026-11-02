//! Construction-only child declarations sharing immutable parent route metadata.

use super::super::nested::{NestedOwner, RecipeOwner};
use super::{
    Binding, BorrowPath, Carriers, Descriptor, EntryMount, Hash, Local, LocalMountSite,
    LocalOptionalMount, MountDeclaration, MountIdentity, OptionalPath, Rc, ScopeToken,
    attach_descriptor, fmt, local,
};
use crate::store::mounted::erased_path::LocalPath;
use crate::store::mounted::path::ComposedPath;

/// Declares children for one installed parent incarnation.
///
/// Before a recipe starts, its parent must still be selectable. If selection is
/// absent, discovery stays pending until a later structural transaction can
/// select the parent. A recipe that returns normally is not repeated for the
/// same parent incarnation.
pub struct MountChildren<'a, R, A, S, X> {
    /// Shares the installed parent's route across sibling declarations.
    path: &'a LocalPath<R, S>,
    /// Exposes weak acquisition and handler identity for this parent.
    site: &'a LocalMountSite<R, S, X, A>,
    /// Attaches each discovered declaration to this live parent incarnation.
    parent: ScopeToken,
    /// Locates this parent in the actor's preorder declaration roster.
    depth: usize,
    /// Borrows root admission metadata without retaining the Store owner.
    binding: &'a Binding<R, A, Local>,
    /// Receives declarations in recipe order for subsequent structural traversal.
    discovered: &'a mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
}

impl<R, A, S, X> fmt::Debug for MountChildren<'_, R, A, S, X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountChildren")
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl<R: 'static, A: 'static, S: 'static, X: 'static> MountChildren<'_, R, A, S, X> {
    /// Borrows the actual weak site of this installed parent declaration.
    #[must_use]
    pub fn site(&self) -> &LocalMountSite<R, S, X, A> {
        self.site
    }

    /// Declares an owner-local ordinary input for this installed incarnation.
    pub fn command<C: 'static>(
        &mut self,
        mapper: impl FnMut(C) -> X + 'static,
    ) -> crate::local::CommandSite<R, C> {
        let (owner, site) = super::commands::attach(
            self.site,
            self.binding,
            mapper,
            crate::store::mounted::InputClass::Ordinary,
        );
        self.push(owner);
        site
    }

    /// Declares control on this same incarnation; resulting work stays ordinary.
    pub fn control<C: 'static>(
        &mut self,
        mapper: impl FnMut(C) -> X + 'static,
    ) -> crate::local::CommandSite<R, C> {
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
        fold: &crate::local::Fold<R, K>,
        value: impl Fn(&S) -> K::Value + 'static,
    ) {
        self.push(fold.attach(self.site, Some(Box::new(value)), None));
    }

    /// Declares a contribution and transforms only this incarnation's subtree.
    ///
    /// # Panics
    /// Panics if this incarnation already declares the same channel.
    pub fn contribute_transformed<K: crate::FoldKey>(
        &mut self,
        fold: &crate::local::Fold<R, K>,
        value: impl Fn(&S) -> K::Value + 'static,
        transform: impl Fn(K::Value) -> K::Value + 'static,
    ) {
        self.push(fold.attach(self.site, Some(Box::new(value)), Some(Box::new(transform))));
    }

    /// Transforms this incarnation's subtree without adding a contribution.
    ///
    /// # Panics
    /// Panics if this incarnation already declares the same channel.
    pub fn transform_fold<K: crate::FoldKey>(
        &mut self,
        fold: &crate::local::Fold<R, K>,
        transform: impl Fn(K::Value) -> K::Value + 'static,
    ) {
        self.push(fold.attach(self.site, None, Some(Box::new(transform))));
    }

    /// Declares every keyed entry with a fresh, owned Feature and callbacks.
    ///
    /// Reordering retains Features, caches and sites. A new entry stamp or
    /// explicit scope leave creates fresh construction; configuration changes
    /// retain the Feature but expire old authority. All callbacks remain local.
    pub fn keyed<F, K, ChildState>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> EntryMount<F> + 'static,
    ) where
        F: local::Feature<State = ChildState> + 'static,
        ChildState: 'static,
        F::Action: 'static,
        K: Eq + Hash + Clone + 'static,
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
    /// Drain all departing subtrees of this collection, construct new entries
    /// in keyed order, then visit the retained/new roster depth-first. Nested
    /// retained collections reconcile during that traversal, not in a global
    /// retirement pass. Reorder/configuration retain Features and recipes;
    /// replacement or explicit scope leave creates them afresh. Returned
    /// callbacks must own any ID or State data they retain from `make`.
    ///
    /// # Examples
    ///
    /// The returned recipe may own an ID and local `Rc` data from the row.
    ///
    /// ```
    /// use rca_core::{KeyedState, local::{EntryMount, MountChildren}};
    /// use std::{cell::Cell, rc::Rc};
    /// # use rca_core::local::{Feature, FeatureContext};
    /// # struct RowFeature(Rc<Cell<usize>>);
    /// # impl Feature for RowFeature {
    /// #     type State = Rc<Cell<usize>>;
    /// #     type Action = ();
    /// #     fn update(&mut self, state: &mut Self::State, _: &(), _: &mut FeatureContext<'_, Self::State, ()>) { self.0.set(state.get()); }
    /// # }
    /// # type Rows = KeyedState<String, Rc<Cell<usize>>>;
    /// # fn declare(children: &mut MountChildren<'_, Rows, (), Rows, ()>) {
    /// children.keyed_with_children(|rows| rows, |rows| rows, |id, row| {
    ///     let id = id.clone();
    ///     let seed = Rc::clone(row);
    ///     (EntryMount::new(RowFeature(Rc::clone(&seed))), move |_, children| {
    ///         let _ = (&id, &seed, children.site());
    ///     })
    /// });
    /// # }
    /// ```
    pub fn keyed_with_children<F, K, G, ChildState>(
        &mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> (EntryMount<F>, G) + 'static,
    ) where
        F: local::Feature<State = ChildState> + 'static,
        ChildState: 'static,
        F::Action: 'static,
        K: Eq + Hash + Clone + 'static,
        G: FnMut(&ChildState, &mut MountChildren<'_, R, A, ChildState, F::Action>) + 'static,
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
        make: impl FnMut(&K, &F::State) -> (EntryMount<F>, G) + 'static,
    ) where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
        K: Eq + Hash + Clone + 'static,
        G: FnMut(
                &F::State,
                &mut MountChildren<'_, R, A, F::State, F::Action>,
                crate::local::RemovalSite<S, F::State, X>,
            ) + 'static,
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
        make: impl FnMut(&crate::StackId, &F::State) -> (EntryMount<F>, G) + 'static,
    ) where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
        G: FnMut(
                &F::State,
                &mut MountChildren<'_, R, A, F::State, F::Action>,
                crate::local::RemovalSite<S, F::State, X>,
            ) + 'static,
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
    pub fn output<O: 'static>(
        &mut self,
        source: &LocalMountSite<R, impl Sized + 'static, impl Sized + 'static, A>,
        configure: impl FnOnce(&mut crate::store::output::local::OutputHandlers<R, A, O>),
    ) -> crate::store::output::local::Output<O> {
        let (owner, output) =
            crate::store::output::local::attach_owner(source.output_registration(), configure);
        self.push(owner);
        output
    }

    /// Declares an optional child with checked owner-local removal authority.
    ///
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
        mut identity: impl FnMut(&S) -> Option<MountIdentity<K>> + 'static,
        make: impl FnMut(&F::State) -> F + 'static,
    ) -> (
        LocalMountSite<R, F::State, F::Action, A>,
        crate::local::RemovalSite<S, F::State, X>,
    )
    where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
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
        let removal = crate::store::removal::local::optional(
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
        descriptor: LocalOptionalMount<S, F, K>,
    ) -> LocalMountSite<R, F::State, F::Action, A>
    where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
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
        descriptor: LocalOptionalMount<S, F, K>,
        recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + 'static,
    ) -> LocalMountSite<R, F::State, F::Action, A>
    where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
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
        descriptor: LocalOptionalMount<S, F, K>,
    ) -> Descriptor<R, F, K, ComposedPath<LocalPath<R, S>, OptionalPath<S, F::State>>>
    where
        F: local::Feature,
        K: 'static,
    {
        let LocalOptionalMount {
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
    fn push(&mut self, inner: impl MountDeclaration<R, A, Local> + 'static) {
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

/// Adds weak owning-removal sites to each keyed entry's one-shot child recipe.
pub(in crate::store) fn keyed_removable<R, A, S, X, F, K, P, G>(
    path: P,
    parent: std::rc::Weak<dyn crate::store::output::local::HandlerRegistration<R, S, X, A>>,
    read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, F::State>,
    write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, F::State>,
    depth: usize,
    mut make: impl FnMut(&K, &F::State) -> (EntryMount<F>, G) + 'static,
) -> impl MountDeclaration<R, A, Local>
where
    R: 'static,
    A: 'static,
    S: 'static,
    X: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + 'static,
    P: BorrowPath<R, State = S> + 'static,
    G: FnMut(
            &F::State,
            &mut MountChildren<'_, R, A, F::State, F::Action>,
            crate::local::RemovalSite<S, F::State, X>,
        ) + 'static,
{
    keyed(path, read, write, depth, move |key, state, stamp| {
        let (entry, mut recipe) = make(key, state);
        let (key, stamp, parent) = (Rc::clone(key), stamp.clone(), std::rc::Weak::clone(&parent));
        (
            entry,
            Some(
                move |state: &F::State,
                      children: &mut MountChildren<'_, R, A, F::State, F::Action>| {
                    let removal = crate::store::removal::local::keyed(
                        std::rc::Weak::clone(&parent),
                        children.site.output_registration(),
                        std::sync::Weak::clone(&children.binding.ingress),
                        write,
                        Rc::clone(&key),
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
    parent: std::rc::Weak<dyn crate::store::output::local::HandlerRegistration<R, S, X, A>>,
    read: for<'s> fn(&'s S) -> &'s crate::StackState<F::State>,
    write: for<'s> fn(&'s mut S) -> &'s mut crate::StackState<F::State>,
    depth: usize,
    mut make: impl FnMut(&crate::StackId, &F::State) -> (EntryMount<F>, G) + 'static,
) -> impl MountDeclaration<R, A, Local>
where
    R: 'static,
    A: 'static,
    S: 'static,
    X: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    P: BorrowPath<R, State = S> + 'static,
    G: FnMut(
            &F::State,
            &mut MountChildren<'_, R, A, F::State, F::Action>,
            crate::local::RemovalSite<S, F::State, X>,
        ) + 'static,
{
    let path = ComposedPath::new(
        path,
        crate::store::mounted::path::StackContainerPath { read, write },
    );
    keyed(
        path,
        |entries| entries,
        |entries| entries,
        depth,
        move |key, state, stamp| {
            let (entry, mut recipe) = make(key, state);
            let (key, stamp, parent) =
                (Rc::clone(key), stamp.clone(), std::rc::Weak::clone(&parent));
            (
                entry,
                Some(
                    move |state: &F::State,
                          children: &mut MountChildren<'_, R, A, F::State, F::Action>| {
                        let removal = crate::store::removal::local::stack(
                            std::rc::Weak::clone(&parent),
                            children.site.output_registration(),
                            std::sync::Weak::clone(&children.binding.ingress),
                            write,
                            Rc::clone(&key),
                            stamp.clone(),
                        );
                        recipe(state, children, removal);
                    },
                ),
            )
        },
    )
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
        &Rc<K>,
        &F::State,
        &crate::composition::EntryStamp,
    ) -> (EntryMount<F>, Option<G>)
    + 'static,
) -> impl MountDeclaration<R, A, Local>
where
    R: 'static,
    A: 'static,
    S: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + 'static,
    P: BorrowPath<R, State = S> + 'static,
    G: FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + 'static,
{
    let entry_path = path.clone();
    super::super::collection::Collection::new(
        path,
        read,
        depth,
        move |key: &K,
              state: &F::State,
              stamp: crate::composition::EntryStamp,
              parent,
              depth,
              binding: &Binding<R, A, Local>|
              -> Option<Box<<Local as Carriers<R, A>>::Mount>> {
            let live = || {
                binding
                    .lifecycle
                    .upgrade()
                    .is_some_and(|lifecycle| !lifecycle.is_closed())
            };
            let key = Rc::new(key.clone());
            if !live() {
                return None;
            }
            let (
                EntryMount {
                    feature,
                    initial,
                    reactions,
                    mut configuration,
                },
                recipe,
            ) = make(&key, state, &stamp);
            if !live() {
                return None;
            }
            let path = ComposedPath::new(
                entry_path.clone(),
                crate::store::mounted::path::KeyedPath::new(read, write, key),
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
                Box::new(super::super::collection::bind_entry(
                    inner, stamp, parent, depth, binding,
                ))
            } else {
                let (inner, _) = attach_descriptor(descriptor);
                Box::new(super::super::collection::bind_entry(
                    inner, stamp, parent, depth, binding,
                ))
            })
        },
    )
}

/// Attaches a root-relative optional declaration and its deferred child recipe.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration and typed acquisition or removal authority."
)]
pub(in crate::store) fn attach<R, A, F, K>(
    descriptor: LocalOptionalMount<R, F, K>,
    recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + 'static,
) -> (
    Box<dyn MountDeclaration<R, A, Local>>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let LocalOptionalMount {
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
    mut recipe: impl FnMut(&F::State, &mut MountChildren<'_, R, A, F::State, F::Action>) + 'static,
    depth: usize,
) -> (
    impl MountDeclaration<R, A, Local>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
{
    let path = descriptor.path.clone();
    let (inner, site) = attach_descriptor(descriptor);
    let parent_site = site.clone();
    let owner = RecipeOwner::new(
        inner,
        move |root: &R,
              parent,
              _inner_depth,
              binding: &Binding<R, A, Local>,
              discovered: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>| {
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
            let path = LocalPath::new(path.clone());
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
