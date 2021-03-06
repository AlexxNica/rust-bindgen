//! Bindgen's core intermediate representation type.

use super::annotations::Annotations;
use super::context::{BindgenContext, ItemId, PartialType};
use super::derive::{CanDeriveCopy, CanDeriveDebug, CanDeriveDefault};
use super::dot::DotAttributes;
use super::function::Function;
use super::item_kind::ItemKind;
use super::layout::Opaque;
use super::module::Module;
use super::template::AsNamed;
use super::traversal::{EdgeKind, Trace, Tracer};
use super::ty::{TemplateDeclaration, Type, TypeKind};
use clang;
use clang_sys;
use parse::{ClangItemParser, ClangSubItemParser, ParseError, ParseResult};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt::Write;
use std::io;
use std::iter;
use regex;

/// A trait to get the canonical name from an item.
///
/// This is the trait that will eventually isolate all the logic related to name
/// mangling and that kind of stuff.
///
/// This assumes no nested paths, at some point I'll have to make it a more
/// complex thing.
///
/// This name is required to be safe for Rust, that is, is not expected to
/// return any rust keyword from here.
pub trait ItemCanonicalName {
    /// Get the canonical name for this item.
    fn canonical_name(&self, ctx: &BindgenContext) -> String;
}

/// The same, but specifies the path that needs to be followed to reach an item.
///
/// To contrast with canonical_name, here's an example:
///
/// ```c++
/// namespace foo {
///     const BAR = 3;
/// }
/// ```
///
/// For bar, the canonical path is `vec!["foo", "BAR"]`, while the canonical
/// name is just `"BAR"`.
pub trait ItemCanonicalPath {
    /// Get the namespace-aware canonical path for this item. This means that if
    /// namespaces are disabled, you'll get a single item, and otherwise you get
    /// the whole path.
    fn namespace_aware_canonical_path(&self,
                                      ctx: &BindgenContext)
                                      -> Vec<String>;

    /// Get the canonical path for this item.
    fn canonical_path(&self, ctx: &BindgenContext) -> Vec<String>;
}

/// A trait for iterating over an item and its parents and up its ancestor chain
/// up to (but not including) the implicit root module.
pub trait ItemAncestors {
    /// Get an iterable over this item's ancestors.
    fn ancestors<'a, 'b>(&self,
                         ctx: &'a BindgenContext<'b>)
                         -> ItemAncestorsIter<'a, 'b>;
}

cfg_if! {
    if #[cfg(debug_assertions)] {
        type DebugOnlyItemSet = ItemSet;
    } else {
        struct DebugOnlyItemSet;

        impl DebugOnlyItemSet {
            fn new() -> Self {
                DebugOnlyItemSet
            }

            fn contains(&self,_id: &ItemId) -> bool {
                false
            }

            fn insert(&mut self, _id: ItemId) {}
        }
    }
}

/// An iterator over an item and its ancestors.
pub struct ItemAncestorsIter<'a, 'b>
    where 'b: 'a,
{
    item: ItemId,
    ctx: &'a BindgenContext<'b>,
    seen: DebugOnlyItemSet,
}

impl<'a, 'b> ItemAncestorsIter<'a, 'b>
    where 'b: 'a,
{
    fn new(ctx: &'a BindgenContext<'b>, item: ItemId) -> Self {
        ItemAncestorsIter {
            item: item,
            ctx: ctx,
            seen: DebugOnlyItemSet::new(),
        }
    }
}

impl<'a, 'b> Iterator for ItemAncestorsIter<'a, 'b>
    where 'b: 'a,
{
    type Item = ItemId;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.ctx.resolve_item(self.item);

        if item.parent_id() == self.item {
            None
        } else {
            self.item = item.parent_id();

            debug_assert!(!self.seen.contains(&item.id()));
            self.seen.insert(item.id());

            Some(item.id())
        }
    }
}

impl AsNamed for ItemId {
    type Extra = ();

    fn as_named(&self, ctx: &BindgenContext, _: &()) -> Option<ItemId> {
        ctx.resolve_item(*self).as_named(ctx, &())
    }
}

impl AsNamed for Item {
    type Extra = ();

    fn as_named(&self, ctx: &BindgenContext, _: &()) -> Option<ItemId> {
        self.kind.as_named(ctx, self)
    }
}

impl AsNamed for ItemKind {
    type Extra = Item;

    fn as_named(&self, ctx: &BindgenContext, item: &Item) -> Option<ItemId> {
        match *self {
            ItemKind::Type(ref ty) => ty.as_named(ctx, item),
            ItemKind::Module(..) |
            ItemKind::Function(..) |
            ItemKind::Var(..) => None,
        }
    }
}

// Pure convenience
impl ItemCanonicalName for ItemId {
    fn canonical_name(&self, ctx: &BindgenContext) -> String {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        ctx.resolve_item(*self).canonical_name(ctx)
    }
}

impl ItemCanonicalPath for ItemId {
    fn namespace_aware_canonical_path(&self,
                                      ctx: &BindgenContext)
                                      -> Vec<String> {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        ctx.resolve_item(*self).namespace_aware_canonical_path(ctx)
    }

    fn canonical_path(&self, ctx: &BindgenContext) -> Vec<String> {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        ctx.resolve_item(*self).canonical_path(ctx)
    }
}

impl ItemAncestors for ItemId {
    fn ancestors<'a, 'b>(&self,
                         ctx: &'a BindgenContext<'b>)
                         -> ItemAncestorsIter<'a, 'b> {
        ItemAncestorsIter::new(ctx, *self)
    }
}

impl ItemAncestors for Item {
    fn ancestors<'a, 'b>(&self,
                         ctx: &'a BindgenContext<'b>)
                         -> ItemAncestorsIter<'a, 'b> {
        self.id().ancestors(ctx)
    }
}

impl Trace for ItemId {
    type Extra = ();

    fn trace<T>(&self, ctx: &BindgenContext, tracer: &mut T, extra: &())
        where T: Tracer,
    {
        ctx.resolve_item(*self).trace(ctx, tracer, extra);
    }
}

impl Trace for Item {
    type Extra = ();

    fn trace<T>(&self, ctx: &BindgenContext, tracer: &mut T, _extra: &())
        where T: Tracer,
    {
        if self.is_hidden(ctx) {
            return;
        }

        match *self.kind() {
            ItemKind::Type(ref ty) => {
                // There are some types, like resolved type references, where we
                // don't want to stop collecting types even though they may be
                // opaque.
                if ty.should_be_traced_unconditionally() ||
                   !self.is_opaque(ctx) {
                    ty.trace(ctx, tracer, self);
                }
            }
            ItemKind::Function(ref fun) => {
                // Just the same way, it has not real meaning for a function to
                // be opaque, so we trace across it.
                tracer.visit(fun.signature());
            }
            ItemKind::Var(ref var) => {
                tracer.visit_kind(var.ty(), EdgeKind::VarType);
            }
            ItemKind::Module(_) => {
                // Module -> children edges are "weak", and we do not want to
                // trace them. If we did, then whitelisting wouldn't work as
                // expected: everything in every module would end up
                // whitelisted.
                //
                // TODO: make a new edge kind for module -> children edges and
                // filter them during whitelisting traversals.
            }
        }
    }
}

impl CanDeriveDebug for Item {
    type Extra = ();

    fn can_derive_debug(&self, ctx: &BindgenContext, _: ()) -> bool {
        if self.detect_derive_debug_cycle.get() {
            return true;
        }

        self.detect_derive_debug_cycle.set(true);

        let result = ctx.options().derive_debug &&
                     match self.kind {
            ItemKind::Type(ref ty) => {
                if self.is_opaque(ctx) {
                    ty.layout(ctx)
                        .map_or(true, |l| l.opaque().can_derive_debug(ctx, ()))
                } else {
                    ty.can_derive_debug(ctx, ())
                }
            }
            _ => false,
        };

        self.detect_derive_debug_cycle.set(false);

        result
    }
}

impl CanDeriveDefault for Item {
    type Extra = ();

    fn can_derive_default(&self, ctx: &BindgenContext, _: ()) -> bool {
        ctx.options().derive_default &&
        match self.kind {
            ItemKind::Type(ref ty) => {
                if self.is_opaque(ctx) {
                    ty.layout(ctx)
                        .map_or(false,
                                |l| l.opaque().can_derive_default(ctx, ()))
                } else {
                    ty.can_derive_default(ctx, ())
                }
            }
            _ => false,
        }
    }
}

impl<'a> CanDeriveCopy<'a> for Item {
    type Extra = ();

    fn can_derive_copy(&self, ctx: &BindgenContext, _: ()) -> bool {
        if self.detect_derive_copy_cycle.get() {
            return true;
        }

        self.detect_derive_copy_cycle.set(true);

        let result = match self.kind {
            ItemKind::Type(ref ty) => {
                if self.is_opaque(ctx) {
                    ty.layout(ctx)
                        .map_or(true, |l| l.opaque().can_derive_copy(ctx, ()))
                } else {
                    ty.can_derive_copy(ctx, self)
                }
            }
            _ => false,
        };

        self.detect_derive_copy_cycle.set(false);

        result
    }

    fn can_derive_copy_in_array(&self, ctx: &BindgenContext, _: ()) -> bool {
        match self.kind {
            ItemKind::Type(ref ty) => {
                if self.is_opaque(ctx) {
                    ty.layout(ctx)
                        .map_or(true, |l| {
                            l.opaque().can_derive_copy_in_array(ctx, ())
                        })
                } else {
                    ty.can_derive_copy_in_array(ctx, self)
                }
            }
            _ => false,
        }
    }
}

/// An item is the base of the bindgen representation, it can be either a
/// module, a type, a function, or a variable (see `ItemKind` for more
/// information).
///
/// Items refer to each other by `ItemId`. Every item has its parent's
/// id. Depending on the kind of item this is, it may also refer to other items,
/// such as a compound type item referring to other types. Collectively, these
/// references form a graph.
///
/// The entry-point to this graph is the "root module": a meta-item used to hold
/// all top-level items.
///
/// An item may have a comment, and annotations (see the `annotations` module).
///
/// Note that even though we parse all the types of annotations in comments, not
/// all of them apply to every item. Those rules are described in the
/// `annotations` module.
#[derive(Debug)]
pub struct Item {
    /// This item's id.
    id: ItemId,

    /// The item's local id, unique only amongst its siblings.  Only used for
    /// anonymous items.
    ///
    /// Lazily initialized in local_id().
    ///
    /// Note that only structs, unions, and enums get a local type id. In any
    /// case this is an implementation detail.
    local_id: Cell<Option<usize>>,

    /// The next local id to use for a child..
    next_child_local_id: Cell<usize>,

    /// A cached copy of the canonical name, as returned by `canonical_name`.
    ///
    /// This is a fairly used operation during codegen so this makes bindgen
    /// considerably faster in those cases.
    canonical_name_cache: RefCell<Option<String>>,

    /// A doc comment over the item, if any.
    comment: Option<String>,
    /// Annotations extracted from the doc comment, or the default ones
    /// otherwise.
    annotations: Annotations,
    /// An item's parent id. This will most likely be a class where this item
    /// was declared, or a module, etc.
    ///
    /// All the items have a parent, except the root module, in which case the
    /// parent id is its own id.
    parent_id: ItemId,
    /// The item kind.
    kind: ItemKind,
    /// Detect cycles when determining if we can derive debug/copy or not, and
    /// avoid infinite recursion.
    detect_derive_debug_cycle: Cell<bool>,
    detect_derive_copy_cycle: Cell<bool>,
}

impl AsRef<ItemId> for Item {
    fn as_ref(&self) -> &ItemId {
        &self.id
    }
}

impl Item {
    /// Construct a new `Item`.
    pub fn new(id: ItemId,
               comment: Option<String>,
               annotations: Option<Annotations>,
               parent_id: ItemId,
               kind: ItemKind)
               -> Self {
        debug_assert!(id != parent_id || kind.is_module());
        Item {
            id: id,
            local_id: Cell::new(None),
            next_child_local_id: Cell::new(1),
            canonical_name_cache: RefCell::new(None),
            parent_id: parent_id,
            comment: comment,
            annotations: annotations.unwrap_or_default(),
            kind: kind,
            detect_derive_debug_cycle: Cell::new(false),
            detect_derive_copy_cycle: Cell::new(false),
        }
    }

    fn new_opaque_type(with_id: ItemId,
                       ty: &clang::Type,
                       ctx: &mut BindgenContext)
                       -> ItemId {
        let ty = Opaque::from_clang_ty(ty);
        let kind = ItemKind::Type(ty);
        let parent = ctx.root_module();
        ctx.add_item(Item::new(with_id, None, None, parent, kind), None, None);
        with_id
    }

    /// Get this `Item`'s identifier.
    pub fn id(&self) -> ItemId {
        self.id
    }

    /// Get this `Item`'s parent's identifier.
    ///
    /// For the root module, the parent's ID is its own ID.
    pub fn parent_id(&self) -> ItemId {
        self.parent_id
    }

    /// Set this item's parent id.
    ///
    /// This is only used so replacements get generated in the proper module.
    pub fn set_parent_for_replacement(&mut self, id: ItemId) {
        self.parent_id = id;
    }

    /// Get this `Item`'s comment, if it has any.
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_ref().map(|c| &**c)
    }

    /// What kind of item is this?
    pub fn kind(&self) -> &ItemKind {
        &self.kind
    }

    /// Get a mutable reference to this item's kind.
    pub fn kind_mut(&mut self) -> &mut ItemKind {
        &mut self.kind
    }

    /// Get an identifier that differentiates this item from its siblings.
    ///
    /// This should stay relatively stable in the face of code motion outside or
    /// below this item's lexical scope, meaning that this can be useful for
    /// generating relatively stable identifiers within a scope.
    pub fn local_id(&self, ctx: &BindgenContext) -> usize {
        if self.local_id.get().is_none() {
            let parent = ctx.resolve_item(self.parent_id);
            let local_id = parent.next_child_local_id.get();
            parent.next_child_local_id.set(local_id + 1);
            self.local_id.set(Some(local_id));
        }
        self.local_id.get().unwrap()
    }

    /// Returns whether this item is a top-level item, from the point of view of
    /// bindgen.
    ///
    /// This point of view changes depending on whether namespaces are enabled
    /// or not. That way, in the following example:
    ///
    /// ```c++
    /// namespace foo {
    ///     static int var;
    /// }
    /// ```
    ///
    /// `var` would be a toplevel item if namespaces are disabled, but won't if
    /// they aren't.
    ///
    /// This function is used to determine when the codegen phase should call
    /// `codegen` on an item, since any item that is not top-level will be
    /// generated by its parent.
    pub fn is_toplevel(&self, ctx: &BindgenContext) -> bool {
        // FIXME: Workaround for some types falling behind when parsing weird
        // stl classes, for example.
        if ctx.options().enable_cxx_namespaces && self.kind().is_module() &&
           self.id() != ctx.root_module() {
            return false;
        }

        let mut parent = self.parent_id;
        loop {
            let parent_item = match ctx.resolve_item_fallible(parent) {
                Some(item) => item,
                None => return false,
            };

            if parent_item.id() == ctx.root_module() {
                return true;
            } else if ctx.options().enable_cxx_namespaces ||
                      !parent_item.kind().is_module() {
                return false;
            }

            parent = parent_item.parent_id();
        }
    }

    /// Get a reference to this item's underlying `Type`. Panic if this is some
    /// other kind of item.
    pub fn expect_type(&self) -> &Type {
        self.kind().expect_type()
    }

    /// Get a reference to this item's underlying `Type`, or `None` if this is
    /// some other kind of item.
    pub fn as_type(&self) -> Option<&Type> {
        self.kind().as_type()
    }

    /// Get a reference to this item's underlying `Function`. Panic if this is
    /// some other kind of item.
    pub fn expect_function(&self) -> &Function {
        self.kind().expect_function()
    }

    /// Is this item a module?
    pub fn is_module(&self) -> bool {
        match self.kind {
            ItemKind::Module(..) => true,
            _ => false,
        }
    }

    /// Get this item's annotations.
    pub fn annotations(&self) -> &Annotations {
        &self.annotations
    }

    /// Whether this item should be hidden.
    ///
    /// This may be due to either annotations or to other kind of configuration.
    pub fn is_hidden(&self, ctx: &BindgenContext) -> bool {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        self.annotations.hide() ||
        ctx.hidden_by_name(&self.canonical_path(ctx), self.id)
    }

    /// Is this item opaque?
    pub fn is_opaque(&self, ctx: &BindgenContext) -> bool {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        self.annotations.opaque() ||
        self.as_type().map_or(false, |ty| ty.is_opaque()) ||
        ctx.opaque_by_name(&self.canonical_path(ctx))
    }

    /// Is this a reference to another type?
    pub fn is_type_ref(&self) -> bool {
        self.as_type().map_or(false, |ty| ty.is_type_ref())
    }

    /// Is this item a var type?
    pub fn is_var(&self) -> bool {
        match *self.kind() {
            ItemKind::Var(..) => true,
            _ => false,
        }
    }

    /// Take out item NameOptions
    pub fn name<'item, 'ctx>(&'item self,
                             ctx: &'item BindgenContext<'ctx>)
                             -> NameOptions<'item, 'ctx> {
        NameOptions::new(self, ctx)
    }

    /// Get the target item id for name generation.
    fn name_target(&self, ctx: &BindgenContext) -> ItemId {
        let mut targets_seen = DebugOnlyItemSet::new();
        let mut item = self;

        loop {
            debug_assert!(!targets_seen.contains(&item.id()));
            targets_seen.insert(item.id());

            if self.annotations().use_instead_of().is_some() {
                return self.id();
            }

            match *item.kind() {
                ItemKind::Type(ref ty) => {
                    match *ty.kind() {
                        TypeKind::ResolvedTypeRef(inner) => {
                            item = ctx.resolve_item(inner);
                        }
                        TypeKind::TemplateInstantiation(ref inst) => {
                            item = ctx.resolve_item(inst.template_definition());
                        }
                        _ => return item.id(),
                    }
                }
                _ => return item.id(),
            }
        }
    }

    /// Get this function item's name, or `None` if this item is not a function.
    fn func_name(&self) -> Option<&str> {
        match *self.kind() {
            ItemKind::Function(ref func) => Some(func.name()),
            _ => None,
        }
    }

    /// Get the overload index for this method. If this is not a method, return
    /// `None`.
    fn overload_index(&self, ctx: &BindgenContext) -> Option<usize> {
        self.func_name().and_then(|func_name| {
            let parent = ctx.resolve_item(self.parent_id());
            if let ItemKind::Type(ref ty) = *parent.kind() {
                if let TypeKind::Comp(ref ci) = *ty.kind() {
                    // All the constructors have the same name, so no need to
                    // resolve and check.
                    return ci.constructors()
                        .iter()
                        .position(|c| *c == self.id())
                        .or_else(|| {
                            ci.methods()
                                .iter()
                                .filter(|m| {
                                    let item = ctx.resolve_item(m.signature());
                                    let func = item.expect_function();
                                    func.name() == func_name
                                })
                                .position(|m| m.signature() == self.id())
                        });
                }
            }

            None
        })
    }

    /// Get this item's base name (aka non-namespaced name).
    fn base_name(&self, ctx: &BindgenContext) -> String {
        if let Some(path) = self.annotations().use_instead_of() {
            return path.last().unwrap().clone();
        }

        match *self.kind() {
            ItemKind::Var(ref var) => var.name().to_owned(),
            ItemKind::Module(ref module) => {
                module.name()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| {
                        format!("_bindgen_mod_{}", self.exposed_id(ctx))
                    })
            }
            ItemKind::Type(ref ty) => {
                let name = match *ty.kind() {
                    TypeKind::ResolvedTypeRef(..) => panic!("should have resolved this in name_target()"),
                    _ => ty.name(),
                };
                name.map(ToOwned::to_owned)
                    .unwrap_or_else(|| {
                        format!("_bindgen_ty_{}", self.exposed_id(ctx))
                    })
            }
            ItemKind::Function(ref fun) => {
                let mut name = fun.name().to_owned();

                if let Some(idx) = self.overload_index(ctx) {
                    if idx > 0 {
                        write!(&mut name, "{}", idx).unwrap();
                    }
                }

                name
            }
        }
    }

    /// Get the canonical name without taking into account the replaces
    /// annotation.
    ///
    /// This is the base logic used to implement hiding and replacing via
    /// annotations, and also to implement proper name mangling.
    ///
    /// The idea is that each generated type in the same "level" (read: module
    /// or namespace) has a unique canonical name.
    ///
    /// This name should be derived from the immutable state contained in the
    /// type and the parent chain, since it should be consistent.
    pub fn real_canonical_name(&self,
                               ctx: &BindgenContext,
                               opt: &NameOptions)
                               -> String {
        let target = ctx.resolve_item(self.name_target(ctx));

        // Short-circuit if the target has an override, and just use that.
        if let Some(path) = target.annotations.use_instead_of() {
            if ctx.options().enable_cxx_namespaces {
                return path.last().unwrap().clone();
            }
            return path.join("_").to_owned();
        }

        let base_name = target.base_name(ctx);

        // Named template type arguments are never namespaced, and never
        // mangled.
        if target.is_named(ctx, &()) {
            return base_name;
        }

        // Concatenate this item's ancestors' names together.
        let mut names: Vec<_> = target.parent_id()
            .ancestors(ctx)
            .filter(|id| *id != ctx.root_module())
            .take_while(|id| {
                // Stop iterating ancestors once we reach a namespace.
                !opt.within_namespaces || !ctx.resolve_item(*id).is_module()
            })
            .map(|id| {
                let item = ctx.resolve_item(id);
                let target = ctx.resolve_item(item.name_target(ctx));
                target.base_name(ctx)
            })
            .filter(|name| !name.is_empty())
            .collect();

        names.reverse();

        if !base_name.is_empty() {
            names.push(base_name);
        }

        let name = names.join("_");

        ctx.rust_mangle(&name).into_owned()
    }

    fn exposed_id(&self, ctx: &BindgenContext) -> String {
        // Only use local ids for enums, classes, structs and union types.  All
        // other items use their global id.
        let ty_kind = self.kind().as_type().map(|t| t.kind());
        if let Some(ty_kind) = ty_kind {
            match *ty_kind {
                TypeKind::Comp(..) |
                TypeKind::Enum(..) => return self.local_id(ctx).to_string(),
                _ => {}
            }
        }

        // Note that this `id_` prefix prevents (really unlikely) collisions
        // between the global id and the local id of an item with the same
        // parent.
        format!("id_{}", self.id().as_usize())
    }

    /// Get a reference to this item's `Module`, or `None` if this is not a
    /// `Module` item.
    pub fn as_module(&self) -> Option<&Module> {
        match self.kind {
            ItemKind::Module(ref module) => Some(module),
            _ => None,
        }
    }

    /// Get a mutable reference to this item's `Module`, or `None` if this is
    /// not a `Module` item.
    pub fn as_module_mut(&mut self) -> Option<&mut Module> {
        match self.kind {
            ItemKind::Module(ref mut module) => Some(module),
            _ => None,
        }
    }
}

/// A set of items.
pub type ItemSet = BTreeSet<ItemId>;

impl DotAttributes for Item {
    fn dot_attributes<W>(&self,
                         ctx: &BindgenContext,
                         out: &mut W)
                         -> io::Result<()>
        where W: io::Write,
    {
        try!(writeln!(out,
                      "<tr><td>{:?}</td></tr>
                       <tr><td>name</td><td>{}</td></tr>",
                      self.id,
                      self.name(ctx).get()));
        self.kind.dot_attributes(ctx, out)
    }
}

impl TemplateDeclaration for ItemId {
    fn self_template_params(&self,
                            ctx: &BindgenContext)
                            -> Option<Vec<ItemId>> {
        ctx.resolve_item_fallible(*self)
            .and_then(|item| item.self_template_params(ctx))
    }
}

impl TemplateDeclaration for Item {
    fn self_template_params(&self,
                            ctx: &BindgenContext)
                            -> Option<Vec<ItemId>> {
        self.kind.self_template_params(ctx)
    }
}

impl TemplateDeclaration for ItemKind {
    fn self_template_params(&self,
                            ctx: &BindgenContext)
                            -> Option<Vec<ItemId>> {
        match *self {
            ItemKind::Type(ref ty) => ty.self_template_params(ctx),
            // If we start emitting bindings to explicitly instantiated
            // functions, then we'll need to check ItemKind::Function for
            // template params.
            ItemKind::Function(_) |
            ItemKind::Module(_) |
            ItemKind::Var(_) => None,
        }
    }
}

// An utility function to handle recursing inside nested types.
fn visit_child(cur: clang::Cursor,
               id: ItemId,
               ty: &clang::Type,
               parent_id: Option<ItemId>,
               ctx: &mut BindgenContext,
               result: &mut Result<ItemId, ParseError>)
               -> clang_sys::CXChildVisitResult {
    use clang_sys::*;
    if result.is_ok() {
        return CXChildVisit_Break;
    }

    *result = Item::from_ty_with_id(id, ty, cur, parent_id, ctx);

    match *result {
        Ok(..) => CXChildVisit_Break,
        Err(ParseError::Recurse) => {
            cur.visit(|c| visit_child(c, id, ty, parent_id, ctx, result));
            CXChildVisit_Continue
        }
        Err(ParseError::Continue) => CXChildVisit_Continue,
    }
}

impl ClangItemParser for Item {
    fn builtin_type(kind: TypeKind,
                    is_const: bool,
                    ctx: &mut BindgenContext)
                    -> ItemId {
        // Feel free to add more here, I'm just lazy.
        match kind {
            TypeKind::Void |
            TypeKind::Int(..) |
            TypeKind::Pointer(..) |
            TypeKind::Float(..) => {}
            _ => panic!("Unsupported builtin type"),
        }

        let ty = Type::new(None, None, kind, is_const);
        let id = ctx.next_item_id();
        let module = ctx.root_module();
        ctx.add_item(Item::new(id, None, None, module, ItemKind::Type(ty)),
                     None,
                     None);
        id
    }


    fn parse(cursor: clang::Cursor,
             parent_id: Option<ItemId>,
             ctx: &mut BindgenContext)
             -> Result<ItemId, ParseError> {
        use ir::function::Function;
        use ir::module::Module;
        use ir::var::Var;
        use clang_sys::*;

        if !cursor.is_valid() {
            return Err(ParseError::Continue);
        }

        let comment = cursor.raw_comment();
        let annotations = Annotations::new(&cursor);

        let current_module = ctx.current_module();
        let relevant_parent_id = parent_id.unwrap_or(current_module);

        macro_rules! try_parse {
            ($what:ident) => {
                match $what::parse(cursor, ctx) {
                    Ok(ParseResult::New(item, declaration)) => {
                        let id = ctx.next_item_id();

                        ctx.add_item(Item::new(id, comment, annotations,
                                               relevant_parent_id,
                                               ItemKind::$what(item)),
                                         declaration,
                                         Some(cursor));
                        return Ok(id);
                    }
                    Ok(ParseResult::AlreadyResolved(id)) => {
                        return Ok(id);
                    }
                    Err(ParseError::Recurse) => return Err(ParseError::Recurse),
                    Err(ParseError::Continue) => {},
                }
            }
        }

        try_parse!(Module);

        // NOTE: Is extremely important to parse functions and vars **before**
        // types.  Otherwise we can parse a function declaration as a type
        // (which is legal), and lose functions to generate.
        //
        // In general, I'm not totally confident this split between
        // ItemKind::Function and TypeKind::FunctionSig is totally worth it, but
        // I guess we can try.
        try_parse!(Function);
        try_parse!(Var);

        // Types are sort of special, so to avoid parsing template classes
        // twice, handle them separately.
        {
            let applicable_cursor = cursor.definition().unwrap_or(cursor);
            match Item::from_ty(&applicable_cursor.cur_type(),
                                applicable_cursor,
                                parent_id,
                                ctx) {
                Ok(ty) => return Ok(ty),
                Err(ParseError::Recurse) => return Err(ParseError::Recurse),
                Err(ParseError::Continue) => {}
            }
        }

        // Guess how does clang treat extern "C" blocks?
        if cursor.kind() == CXCursor_UnexposedDecl {
            Err(ParseError::Recurse)
        } else {
            // We whitelist cursors here known to be unhandled, to prevent being
            // too noisy about this.
            match cursor.kind() {
                CXCursor_MacroDefinition |
                CXCursor_MacroExpansion |
                CXCursor_UsingDeclaration |
                CXCursor_UsingDirective |
                CXCursor_StaticAssert |
                CXCursor_InclusionDirective => {
                    debug!("Unhandled cursor kind {:?}: {:?}",
                           cursor.kind(),
                           cursor);
                }
                _ => {
                    // ignore toplevel operator overloads
                    let spelling = cursor.spelling();
                    if !spelling.starts_with("operator") {
                        error!("Unhandled cursor kind {:?}: {:?}",
                               cursor.kind(),
                               cursor);
                    }
                }
            }

            Err(ParseError::Continue)
        }
    }

    fn from_ty_or_ref(ty: clang::Type,
                      location: clang::Cursor,
                      parent_id: Option<ItemId>,
                      ctx: &mut BindgenContext)
                      -> ItemId {
        let id = ctx.next_item_id();
        Self::from_ty_or_ref_with_id(id, ty, location, parent_id, ctx)
    }

    /// Parse a C++ type. If we find a reference to a type that has not been
    /// defined yet, use `UnresolvedTypeRef` as a placeholder.
    ///
    /// This logic is needed to avoid parsing items with the incorrect parent
    /// and it's sort of complex to explain, so I'll just point to
    /// `tests/headers/typeref.hpp` to see the kind of constructs that forced
    /// this.
    ///
    /// Typerefs are resolved once parsing is completely done, see
    /// `BindgenContext::resolve_typerefs`.
    fn from_ty_or_ref_with_id(potential_id: ItemId,
                              ty: clang::Type,
                              location: clang::Cursor,
                              parent_id: Option<ItemId>,
                              ctx: &mut BindgenContext)
                              -> ItemId {
        debug!("from_ty_or_ref_with_id: {:?} {:?}, {:?}, {:?}",
               potential_id,
               ty,
               location,
               parent_id);

        if ctx.collected_typerefs() {
            debug!("refs already collected, resolving directly");
            return Item::from_ty_with_id(potential_id,
                                         &ty,
                                         location,
                                         parent_id,
                                         ctx)
                .unwrap_or_else(|_| {
                    Item::new_opaque_type(potential_id, &ty, ctx)
                });
        }

        if let Some(ty) = ctx.builtin_or_resolved_ty(potential_id,
                                                     parent_id,
                                                     &ty,
                                                     Some(location)) {
            debug!("{:?} already resolved: {:?}", ty, location);
            return ty;
        }

        debug!("New unresolved type reference: {:?}, {:?}", ty, location);

        let is_const = ty.is_const();
        let kind = TypeKind::UnresolvedTypeRef(ty, location, parent_id);
        let current_module = ctx.current_module();
        ctx.add_item(Item::new(potential_id,
                               None,
                               None,
                               parent_id.unwrap_or(current_module),
                               ItemKind::Type(Type::new(None,
                                                        None,
                                                        kind,
                                                        is_const))),
                     Some(clang::Cursor::null()),
                     None);
        potential_id
    }

    fn from_ty(ty: &clang::Type,
               location: clang::Cursor,
               parent_id: Option<ItemId>,
               ctx: &mut BindgenContext)
               -> Result<ItemId, ParseError> {
        let id = ctx.next_item_id();
        Item::from_ty_with_id(id, ty, location, parent_id, ctx)
    }

    /// This is one of the trickiest methods you'll find (probably along with
    /// some of the ones that handle templates in `BindgenContext`).
    ///
    /// This method parses a type, given the potential id of that type (if
    /// parsing it was correct), an optional location we're scanning, which is
    /// critical some times to obtain information, an optional parent item id,
    /// that will, if it's `None`, become the current module id, and the
    /// context.
    fn from_ty_with_id(id: ItemId,
                       ty: &clang::Type,
                       location: clang::Cursor,
                       parent_id: Option<ItemId>,
                       ctx: &mut BindgenContext)
                       -> Result<ItemId, ParseError> {
        use clang_sys::*;

        debug!("Item::from_ty_with_id: {:?}\n\
                \tty = {:?},\n\
                \tlocation = {:?}",
               id,
               ty,
               location);

        if ty.kind() == clang_sys::CXType_Unexposed ||
           location.cur_type().kind() == clang_sys::CXType_Unexposed {

            if ty.is_associated_type() ||
               location.cur_type().is_associated_type() {
                return Ok(Item::new_opaque_type(id, ty, ctx));
            }

            if let Some(id) = Item::named_type(Some(id), location, ctx) {
                return Ok(id);
            }
        }

        let decl = {
            let decl = ty.declaration();
            decl.definition().unwrap_or(decl)
        };

        let comment = decl.raw_comment()
            .or_else(|| location.raw_comment());
        let annotations = Annotations::new(&decl)
            .or_else(|| Annotations::new(&location));

        if let Some(ref annotations) = annotations {
            if let Some(ref replaced) = annotations.use_instead_of() {
                ctx.replace(replaced, id);
            }
        }

        if let Some(ty) =
            ctx.builtin_or_resolved_ty(id, parent_id, ty, Some(location)) {
            return Ok(ty);
        }

        // First, check we're not recursing.
        let mut valid_decl = decl.kind() != CXCursor_NoDeclFound;
        let declaration_to_look_for = if valid_decl {
            decl.canonical()
        } else if location.kind() ==
                                                CXCursor_ClassTemplate {
            valid_decl = true;
            location
        } else {
            decl
        };

        if valid_decl {
            if let Some(partial) = ctx.currently_parsed_types()
                .iter()
                .find(|ty| *ty.decl() == declaration_to_look_for) {
                debug!("Avoiding recursion parsing type: {:?}", ty);
                return Ok(partial.id());
            }
        }

        let current_module = ctx.current_module();
        let partial_ty = PartialType::new(declaration_to_look_for, id);
        if valid_decl {
            ctx.begin_parsing(partial_ty);
        }

        let result = Type::from_clang_ty(id, ty, location, parent_id, ctx);
        let relevant_parent_id = parent_id.unwrap_or(current_module);
        let ret = match result {
            Ok(ParseResult::AlreadyResolved(ty)) => Ok(ty),
            Ok(ParseResult::New(item, declaration)) => {
                ctx.add_item(Item::new(id,
                                       comment,
                                       annotations,
                                       relevant_parent_id,
                                       ItemKind::Type(item)),
                             declaration,
                             Some(location));
                Ok(id)
            }
            Err(ParseError::Continue) => Err(ParseError::Continue),
            Err(ParseError::Recurse) => {
                debug!("Item::from_ty recursing in the ast");
                let mut result = Err(ParseError::Recurse);

                // Need to pop here, otherwise we'll get stuck.
                //
                // TODO: Find a nicer interface, really. Also, the
                // declaration_to_look_for suspiciously shares a lot of
                // logic with ir::context, so we should refactor that.
                if valid_decl {
                    let finished = ctx.finish_parsing();
                    assert_eq!(*finished.decl(), declaration_to_look_for);
                }

                location.visit(|cur| {
                    visit_child(cur, id, ty, parent_id, ctx, &mut result)
                });

                if valid_decl {
                    let partial_ty = PartialType::new(declaration_to_look_for,
                                                      id);
                    ctx.begin_parsing(partial_ty);
                }

                // If we have recursed into the AST all we know, and we still
                // haven't found what we've got, let's just try and make a named
                // type.
                //
                // This is what happens with some template members, for example.
                if let Err(ParseError::Recurse) = result {
                    warn!("Unknown type, assuming named template type: \
                          id = {:?}; spelling = {}",
                          id,
                          ty.spelling());
                    Item::named_type(Some(id), location, ctx)
                        .map(Ok)
                        .unwrap_or(Err(ParseError::Recurse))
                } else {
                    result
                }
            }
        };

        if valid_decl {
            let partial_ty = ctx.finish_parsing();
            assert_eq!(*partial_ty.decl(), declaration_to_look_for);
        }

        ret
    }

    /// A named type is a template parameter, e.g., the "T" in Foo<T>. They're
    /// always local so it's the only exception when there's no declaration for
    /// a type.
    fn named_type(with_id: Option<ItemId>,
                  location: clang::Cursor,
                  ctx: &mut BindgenContext)
                  -> Option<ItemId> {
        let ty = location.cur_type();

        debug!("Item::named_type:\n\
                \twith_id = {:?},\n\
                \tty = {} {:?},\n\
                \tlocation: {:?}",
               with_id,
               ty.spelling(),
               ty,
               location);

        if ty.kind() != clang_sys::CXType_Unexposed {
            // If the given cursor's type's kind is not Unexposed, then we
            // aren't looking at a template parameter. This check may need to be
            // updated in the future if they start properly exposing template
            // type parameters.
            return None;
        }

        let ty_spelling = ty.spelling();

        // Clang does not expose any information about template type parameters
        // via their clang::Type, nor does it give us their canonical cursors
        // the straightforward way. However, there are three situations from
        // which we can find the definition of the template type parameter, if
        // the cursor is indeed looking at some kind of a template type
        // parameter or use of one:
        //
        // 1. The cursor is pointing at the template type parameter's
        // definition. This is the trivial case.
        //
        //     (kind = TemplateTypeParameter, ...)
        //
        // 2. The cursor is pointing at a TypeRef whose referenced() cursor is
        // situation (1).
        //
        //     (kind = TypeRef,
        //      referenced = (kind = TemplateTypeParameter, ...),
        //      ...)
        //
        // 3. The cursor is pointing at some use of a template type parameter
        // (for example, in a FieldDecl), and this cursor has a child cursor
        // whose spelling is the same as the parent's type's spelling, and whose
        // kind is a TypeRef of the situation (2) variety.
        //
        //    (kind = FieldDecl,
        //     type = (kind = Unexposed,
        //             spelling = "T",
        //             ...),
        //     children =
        //        (kind = TypeRef,
        //         spelling = "T",
        //         referenced = (kind = TemplateTypeParameter,
        //                       spelling = "T",
        //                       ...),
        //         ...)
        //     ...)
        //
        // TODO: The alternative to this hacky pattern matching would be to
        // maintain proper scopes of template parameters while parsing and use
        // de Brujin indices to access template parameters, which clang exposes
        // in the cursor's type's canonical type's spelling:
        // "type-parameter-x-y". That is probably a better approach long-term,
        // but maintaining these scopes properly would require more changes to
        // the whole libclang -> IR parsing code.

        fn is_template_with_spelling(refd: &clang::Cursor,
                                     spelling: &str)
                                     -> bool {
            lazy_static! {
                static ref ANON_TYPE_PARAM_RE: regex::Regex =
                    regex::Regex::new(r"^type\-parameter\-\d+\-\d+$").unwrap();
            }

            if refd.kind() != clang_sys::CXCursor_TemplateTypeParameter {
                return false;
            }

            let refd_spelling = refd.spelling();
            refd_spelling == spelling ||
                // Allow for anonymous template parameters.
                (refd_spelling.is_empty() && ANON_TYPE_PARAM_RE.is_match(spelling.as_ref()))
        }

        let definition = if is_template_with_spelling(&location,
                                                      &ty_spelling) {
            // Situation (1)
            location
        } else if location.kind() ==
                                   clang_sys::CXCursor_TypeRef {
            // Situation (2)
            match location.referenced() {
                Some(refd) if is_template_with_spelling(&refd,
                                                        &ty_spelling) => refd,
                _ => return None,
            }
        } else {
            // Situation (3)
            let mut definition = None;

            location.visit(|child| {
                let child_ty = child.cur_type();
                if child_ty.kind() == clang_sys::CXCursor_TypeRef &&
                   child_ty.spelling() == ty_spelling {
                    match child.referenced() {
                        Some(refd) if is_template_with_spelling(&refd, &ty_spelling) => {
                            definition = Some(refd);
                            return clang_sys::CXChildVisit_Break;
                        }
                        _ => {}
                    }
                }

                clang_sys::CXChildVisit_Continue
            });

            if let Some(def) = definition {
                def
            } else {
                return None;
            }
        };
        assert!(is_template_with_spelling(&definition, &ty_spelling));

        // Named types are always parented to the root module. They are never
        // referenced with namespace prefixes, and they can't inherit anything
        // from their parent either, so it is simplest to just hang them off
        // something we know will always exist.
        let parent = ctx.root_module();

        if let Some(id) = ctx.get_named_type(&definition) {
            if let Some(with_id) = with_id {
                return Some(ctx.build_ty_wrapper(with_id, id, Some(parent), &ty));
            } else {
                return Some(id);
            }
        }

        // See tests/headers/const_tparam.hpp and
        // tests/headers/variadic_tname.hpp.
        let name = ty_spelling.replace("const ", "")
            .replace(".", "");

        let id = with_id.unwrap_or_else(|| ctx.next_item_id());
        let item = Item::new(id,
                             None,
                             None,
                             parent,
                             ItemKind::Type(Type::named(name)));
        ctx.add_named_type(item, definition);
        Some(id)
    }
}

impl ItemCanonicalName for Item {
    fn canonical_name(&self, ctx: &BindgenContext) -> String {
        debug_assert!(ctx.in_codegen_phase(),
                      "You're not supposed to call this yet");
        if self.canonical_name_cache.borrow().is_none() {
            let in_namespace = ctx.options().enable_cxx_namespaces ||
                               ctx.options().disable_name_namespacing;

            *self.canonical_name_cache.borrow_mut() = if in_namespace {
                Some(self.name(ctx).within_namespaces().get())
            } else {
                Some(self.name(ctx).get())
            };
        }
        return self.canonical_name_cache.borrow().as_ref().unwrap().clone();
    }
}

impl ItemCanonicalPath for Item {
    fn namespace_aware_canonical_path(&self,
                                      ctx: &BindgenContext)
                                      -> Vec<String> {
        let path = self.canonical_path(ctx);
        if ctx.options().enable_cxx_namespaces {
            return path;
        }
        if ctx.options().disable_name_namespacing {
            return vec![path.last().unwrap().clone()];
        }
        return vec![path[1..].join("_")];
    }

    fn canonical_path(&self, ctx: &BindgenContext) -> Vec<String> {
        if let Some(path) = self.annotations().use_instead_of() {
            let mut ret =
                vec![ctx.resolve_item(ctx.root_module()).name(ctx).get()];
            ret.extend_from_slice(path);
            return ret;
        }

        let target = ctx.resolve_item(self.name_target(ctx));
        let mut path: Vec<_> = target.ancestors(ctx)
            .chain(iter::once(ctx.root_module()))
            .map(|id| ctx.resolve_item(id))
            .filter(|item| {
                item.id() == target.id() ||
                item.as_module().map_or(false, |module| {
                    !module.is_inline() ||
                    ctx.options().conservative_inline_namespaces
                })
            })
            .map(|item| {
                ctx.resolve_item(item.name_target(ctx))
                    .name(ctx)
                    .within_namespaces()
                    .get()
            })
            .collect();
        path.reverse();
        path
    }
}

/// Builder struct for naming variations, which hold inside different
/// flags for naming options.
#[derive(Debug)]
pub struct NameOptions<'item, 'ctx>
    where 'ctx: 'item,
{
    item: &'item Item,
    ctx: &'item BindgenContext<'ctx>,
    within_namespaces: bool,
}

impl<'item, 'ctx> NameOptions<'item, 'ctx> {
    /// Construct a new `NameOptions`
    pub fn new(item: &'item Item, ctx: &'item BindgenContext<'ctx>) -> Self {
        NameOptions {
            item: item,
            ctx: ctx,
            within_namespaces: false,
        }
    }

    /// Construct the name without the item's containing C++ namespaces mangled
    /// into it. In other words, the item's name within the item's namespace.
    pub fn within_namespaces(&mut self) -> &mut Self {
        self.within_namespaces = true;
        self
    }

    /// Construct a name `String`
    pub fn get(&self) -> String {
        self.item.real_canonical_name(self.ctx, self)
    }
}
