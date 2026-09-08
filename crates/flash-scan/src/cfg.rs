//! What ships: an item gated on `test` in any `cfg` predicate is not
//! part of the program a stranger reaches, and is removed before the
//! model is built. A feature gate is shipped source (the feature may be
//! on), and so is every other predicate. Nothing is cut by text
//! position: a `#[cfg(test)]` on a `use`, a helper or an inline module
//! removes exactly that item and leaves the rest of the file seen.

use syn::punctuated::Punctuated;
use syn::visit_mut::VisitMut;
use syn::{Attribute, Meta, Token};

/// Whether an attribute list admits the item into shipped source.
pub fn shipped(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .all(|attr| !attr.path().is_ident("cfg") || eval_cfg(attr))
}

fn eval_cfg(attr: &Attribute) -> bool {
    let meta: Meta = attr
        .parse_args()
        .unwrap_or_else(|e| panic!("unreadable cfg attribute: {e}"));
    eval(&meta)
}

fn eval(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => !path.is_ident("test"),
        Meta::NameValue(_) => true,
        Meta::List(list) => {
            let nested = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .unwrap_or_else(|e| panic!("unreadable cfg predicate: {e}"));
            match list.path.get_ident().map(ToString::to_string).as_deref() {
                Some("any") => nested.iter().any(eval),
                Some("all") => nested.iter().all(eval),
                Some("not") => !nested.iter().all(eval),
                _ => true,
            }
        }
    }
}

/// Removes every item, member, statement, field, variant, arm and
/// parameter that does not ship.
pub fn strip(file: &mut syn::File) {
    if !shipped(&file.attrs) {
        file.items.clear();
        return;
    }
    Stripper.visit_file_mut(file);
}

struct Stripper;

impl VisitMut for Stripper {
    fn visit_file_mut(&mut self, node: &mut syn::File) {
        node.items.retain(|item| shipped(item_attrs(item)));
        syn::visit_mut::visit_file_mut(self, node);
    }

    fn visit_item_mod_mut(&mut self, node: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut node.content {
            items.retain(|item| shipped(item_attrs(item)));
        }
        syn::visit_mut::visit_item_mod_mut(self, node);
    }

    fn visit_item_impl_mut(&mut self, node: &mut syn::ItemImpl) {
        node.items.retain(|item| shipped(impl_item_attrs(item)));
        syn::visit_mut::visit_item_impl_mut(self, node);
    }

    fn visit_item_trait_mut(&mut self, node: &mut syn::ItemTrait) {
        node.items.retain(|item| shipped(trait_item_attrs(item)));
        syn::visit_mut::visit_item_trait_mut(self, node);
    }

    fn visit_block_mut(&mut self, node: &mut syn::Block) {
        node.stmts.retain(|stmt| shipped(stmt_attrs(stmt)));
        syn::visit_mut::visit_block_mut(self, node);
    }

    fn visit_fields_named_mut(&mut self, node: &mut syn::FieldsNamed) {
        retain_punctuated(&mut node.named, |field| shipped(&field.attrs));
        syn::visit_mut::visit_fields_named_mut(self, node);
    }

    fn visit_fields_unnamed_mut(&mut self, node: &mut syn::FieldsUnnamed) {
        retain_punctuated(&mut node.unnamed, |field| shipped(&field.attrs));
        syn::visit_mut::visit_fields_unnamed_mut(self, node);
    }

    fn visit_item_enum_mut(&mut self, node: &mut syn::ItemEnum) {
        retain_punctuated(&mut node.variants, |variant| shipped(&variant.attrs));
        syn::visit_mut::visit_item_enum_mut(self, node);
    }

    fn visit_expr_match_mut(&mut self, node: &mut syn::ExprMatch) {
        node.arms.retain(|arm| shipped(&arm.attrs));
        syn::visit_mut::visit_expr_match_mut(self, node);
    }

    fn visit_signature_mut(&mut self, node: &mut syn::Signature) {
        retain_punctuated(&mut node.inputs, |arg| match arg {
            syn::FnArg::Receiver(r) => shipped(&r.attrs),
            syn::FnArg::Typed(t) => shipped(&t.attrs),
        });
        syn::visit_mut::visit_signature_mut(self, node);
    }
}

fn retain_punctuated<T, P: Default>(list: &mut Punctuated<T, P>, keep: impl Fn(&T) -> bool) {
    let kept: Vec<T> = std::mem::take(list)
        .into_iter()
        .filter(|t| keep(t))
        .collect();
    list.extend(kept);
}

pub fn item_attrs(item: &syn::Item) -> &[Attribute] {
    match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[Attribute] {
    match item {
        syn::ImplItem::Const(i) => &i.attrs,
        syn::ImplItem::Fn(i) => &i.attrs,
        syn::ImplItem::Type(i) => &i.attrs,
        syn::ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[Attribute] {
    match item {
        syn::TraitItem::Const(i) => &i.attrs,
        syn::TraitItem::Fn(i) => &i.attrs,
        syn::TraitItem::Type(i) => &i.attrs,
        syn::TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn stmt_attrs(stmt: &syn::Stmt) -> &[Attribute] {
    match stmt {
        syn::Stmt::Local(l) => &l.attrs,
        syn::Stmt::Item(i) => item_attrs(i),
        syn::Stmt::Macro(m) => &m.attrs,
        syn::Stmt::Expr(e, _) => expr_attrs(e),
    }
}

/// The attributes of the expression shapes a statement can be; any
/// other shape carrying a false `cfg` is caught by the extractor's
/// attribute check, which refuses it rather than keeping it.
fn expr_attrs(expr: &syn::Expr) -> &[Attribute] {
    match expr {
        syn::Expr::Block(e) => &e.attrs,
        syn::Expr::Call(e) => &e.attrs,
        syn::Expr::If(e) => &e.attrs,
        syn::Expr::Macro(e) => &e.attrs,
        syn::Expr::Match(e) => &e.attrs,
        syn::Expr::MethodCall(e) => &e.attrs,
        syn::Expr::Return(e) => &e.attrs,
        syn::Expr::ForLoop(e) => &e.attrs,
        syn::Expr::While(e) => &e.attrs,
        syn::Expr::Loop(e) => &e.attrs,
        syn::Expr::Assign(e) => &e.attrs,
        syn::Expr::Let(e) => &e.attrs,
        _ => &[],
    }
}
