//! The walk that turns a stripped `syn::File` into a `SourceFile`: one
//! pass, keeping track of the enclosing function, impl, trait, module
//! and statement, and recording what each test may later ask about.

use std::collections::HashMap;
use std::path::PathBuf;

use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::tokens::{compact, idents_in, paths_in, string_literals_in};
use crate::{
    Attr, Call, Field, FnKind, Function, Let, MacroCall, MethodCall, Param, PathRef, SourceFile,
    StringLit, TypeDecl, TypeKind, TypeRef, UseDecl, Vis,
};

pub fn source_file(path: PathBuf, rel: String, ast: &syn::File) -> SourceFile {
    let mut walker = Walker::default();
    walker.visit_file(ast);
    let idents = idents_in(&ast.to_token_stream());
    SourceFile {
        path,
        rel,
        functions: walker.functions,
        types: walker.types,
        uses: walker.uses,
        consts: walker.consts,
        calls: walker.calls,
        method_calls: walker.method_calls,
        macros: walker.macros,
        string_literals: walker.string_literals,
        paths: walker.paths,
        idents,
    }
}

#[derive(Default)]
struct Walker {
    functions: Vec<Function>,
    types: Vec<TypeDecl>,
    uses: Vec<UseDecl>,
    consts: HashMap<String, String>,
    calls: Vec<Call>,
    method_calls: Vec<MethodCall>,
    macros: Vec<MacroCall>,
    string_literals: Vec<StringLit>,
    paths: Vec<PathRef>,
    /// The functions being walked, innermost last.
    frames: Vec<Function>,
    /// The `impl` blocks being walked: (self type, trait).
    impls: Vec<(String, Option<String>)>,
    traits: Vec<String>,
    modules: Vec<String>,
    stmts: Vec<String>,
}

impl Walker {
    fn function(&self) -> Option<String> {
        self.frames.last().map(|f| f.name.clone())
    }

    fn stmt(&self) -> String {
        self.stmts.last().cloned().unwrap_or_default()
    }

    fn enter(&mut self, function: Function) {
        self.frames.push(function);
    }

    fn leave(&mut self) {
        let function = self.frames.pop().expect("a function frame");
        self.functions.push(function);
    }

    fn record_macro(
        &mut self,
        path: Vec<String>,
        name: Option<String>,
        tokens: &proc_macro2::TokenStream,
        line: usize,
    ) {
        let function = self.function();
        for lit in string_literals_in(tokens) {
            self.string_literals.push(StringLit {
                value: lit.value,
                line: lit.line,
                function: function.clone(),
                in_macro: true,
            });
        }
        for path in paths_in(tokens) {
            self.paths.push(PathRef {
                segments: path.segments,
                line: path.line,
                function: function.clone(),
                in_macro: true,
            });
        }
        self.macros.push(MacroCall {
            path,
            name,
            tokens: tokens.clone(),
            line,
            function,
        });
    }

    fn build_function(
        &self,
        name: String,
        kind: FnKind,
        vis: Vis,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        lines: (usize, usize),
    ) -> Function {
        let mut has_self = false;
        let mut params = Vec::new();
        for input in &sig.inputs {
            match input {
                syn::FnArg::Receiver(_) => has_self = true,
                syn::FnArg::Typed(t) => params.push(Param {
                    pat: compact(&t.pat.to_token_stream()),
                    pat_idents: idents_of(&t.pat.to_token_stream()),
                    ty: type_ref(&t.ty),
                }),
            }
        }
        let ret = match &sig.output {
            syn::ReturnType::Default => None,
            syn::ReturnType::Type(_, ty) => Some(type_ref(ty)),
        };
        Function {
            name,
            kind,
            vis,
            is_async: sig.asyncness.is_some(),
            attrs: attrs.iter().filter_map(attr).collect(),
            has_self,
            params,
            ret,
            lets: Vec::new(),
            lines,
            module: self.modules.clone(),
            sig_idents: idents_of(&sig.to_token_stream()),
        }
    }
}

fn idents_of(tokens: &proc_macro2::TokenStream) -> Vec<String> {
    idents_in(tokens).into_iter().map(|i| i.name).collect()
}

fn type_ref(ty: &syn::Type) -> TypeRef {
    let tokens = ty.to_token_stream();
    TypeRef {
        compact: compact(&tokens),
        idents: idents_of(&tokens),
    }
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

fn vis(v: &syn::Visibility) -> Vis {
    match v {
        syn::Visibility::Public(_) => Vis::Pub,
        syn::Visibility::Restricted(r) => Vis::Restricted(compact(&r.path.to_token_stream())),
        syn::Visibility::Inherited => Vis::Private,
    }
}

/// An attribute as the model keeps it; doc comments are not kept.
fn attr(a: &syn::Attribute) -> Option<Attr> {
    let path = path_segments(a.path());
    if path == ["doc"] {
        return None;
    }
    let args = match &a.meta {
        syn::Meta::Path(_) => String::new(),
        syn::Meta::List(l) => compact(&l.tokens),
        syn::Meta::NameValue(nv) => compact(&nv.value.to_token_stream()),
    };
    Some(Attr {
        path,
        args,
        line: a.span().start().line,
    })
}

/// A false `cfg` that survived stripping: refused, never kept.
fn check_cfg(a: &syn::Attribute) {
    if a.path().is_ident("cfg") && !crate::cfg::shipped(std::slice::from_ref(a)) {
        panic!(
            "line {}: a `#[cfg(...)]` the model does not strip would hide code it should read: {}",
            a.span().start().line,
            compact(&a.to_token_stream())
        );
    }
}

fn derives(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for a in attrs {
        if !a.path().is_ident("derive") {
            continue;
        }
        let paths = a
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .unwrap_or_else(|e| panic!("unreadable derive: {e}"));
        out.extend(
            paths
                .iter()
                .filter_map(|p| p.segments.last().map(|s| s.ident.to_string())),
        );
    }
    out
}

fn deserialize_with(attrs: &[syn::Attribute]) -> Option<String> {
    let mut found = None;
    for a in attrs {
        if !a.path().is_ident("serde") {
            continue;
        }
        a.parse_nested_meta(|meta| {
            if meta.path.is_ident("deserialize_with") {
                let value: syn::LitStr = meta.value()?.parse()?;
                found = Some(value.value());
            } else if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            } else if meta.input.peek(syn::token::Paren) {
                meta.parse_nested_meta(|inner| {
                    if inner.input.peek(syn::Token![=]) {
                        let _: syn::Expr = inner.value()?.parse()?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|e| {
            panic!(
                "line {}: unreadable serde attribute: {e}",
                a.span().start().line
            )
        });
    }
    found
}

fn fields(fields: &syn::Fields, variant: Option<&str>) -> Vec<Field> {
    fields
        .iter()
        .map(|f| Field {
            name: f.ident.as_ref().map(ToString::to_string),
            variant: variant.map(str::to_string),
            ty: type_ref(&f.ty),
            attrs: f.attrs.iter().filter_map(attr).collect(),
            deserialize_with: deserialize_with(&f.attrs),
            line: f.span().start().line,
        })
        .collect()
}

fn unparen(expr: &syn::Expr) -> &syn::Expr {
    match expr {
        syn::Expr::Paren(p) => unparen(&p.expr),
        other => other,
    }
}

impl<'ast> Visit<'ast> for Walker {
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        // Attributes are read where their item is built; their literals
        // (doc comments, descriptions) are not code and are not walked.
        check_cfg(node);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.modules.push(node.ident.to_string());
        syn::visit::visit_item_mod(self, node);
        self.modules.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let function = self.build_function(
            node.sig.ident.to_string(),
            FnKind::Free,
            vis(&node.vis),
            &node.attrs,
            &node.sig,
            (node.span().start().line, node.span().end().line),
        );
        self.enter(function);
        syn::visit::visit_item_fn(self, node);
        self.leave();
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let owner = match &*node.self_ty {
            syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default(),
            other => compact(&other.to_token_stream()),
        };
        let trait_ = node
            .trait_
            .as_ref()
            .and_then(|(_, p, _)| p.segments.last().map(|s| s.ident.to_string()));
        self.impls.push((owner, trait_));
        syn::visit::visit_item_impl(self, node);
        self.impls.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let (owner, trait_) = self.impls.last().cloned().expect("inside an impl");
        let function = self.build_function(
            node.sig.ident.to_string(),
            FnKind::Method { owner, trait_ },
            vis(&node.vis),
            &node.attrs,
            &node.sig,
            (node.span().start().line, node.span().end().line),
        );
        self.enter(function);
        syn::visit::visit_impl_item_fn(self, node);
        self.leave();
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.traits.push(node.ident.to_string());
        syn::visit::visit_item_trait(self, node);
        self.traits.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let trait_ = self.traits.last().cloned().expect("inside a trait");
        let function = self.build_function(
            node.sig.ident.to_string(),
            FnKind::TraitDecl {
                trait_,
                has_default: node.default.is_some(),
            },
            Vis::Inherited,
            &node.attrs,
            &node.sig,
            (node.span().start().line, node.span().end().line),
        );
        self.enter(function);
        syn::visit::visit_trait_item_fn(self, node);
        self.leave();
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        self.types.push(TypeDecl {
            name: node.ident.to_string(),
            kind: TypeKind::Struct {
                tuple: matches!(node.fields, syn::Fields::Unnamed(_)),
            },
            vis: vis(&node.vis),
            derives: derives(&node.attrs),
            attrs: node.attrs.iter().filter_map(attr).collect(),
            fields: fields(&node.fields, None),
            line: node.span().start().line,
            module: self.modules.clone(),
        });
        syn::visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let fields = node
            .variants
            .iter()
            .flat_map(|v| fields(&v.fields, Some(&v.ident.to_string())))
            .collect();
        self.types.push(TypeDecl {
            name: node.ident.to_string(),
            kind: TypeKind::Enum,
            vis: vis(&node.vis),
            derives: derives(&node.attrs),
            attrs: node.attrs.iter().filter_map(attr).collect(),
            fields,
            line: node.span().start().line,
            module: self.modules.clone(),
        });
        syn::visit::visit_item_enum(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        self.types.push(TypeDecl {
            name: node.ident.to_string(),
            kind: TypeKind::Union,
            vis: vis(&node.vis),
            derives: derives(&node.attrs),
            attrs: node.attrs.iter().filter_map(attr).collect(),
            fields: fields(&syn::Fields::Named(node.fields.clone()), None),
            line: node.span().start().line,
            module: self.modules.clone(),
        });
        syn::visit::visit_item_union(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = &*node.expr
        {
            self.consts.insert(node.ident.to_string(), s.value());
        }
        syn::visit::visit_item_const(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        let line = node.span().start().line;
        let function = self.function();
        let mut prefix = Vec::new();
        if node.leading_colon.is_some() {
            prefix.push(String::new());
        }
        flatten_use(&node.tree, &mut prefix, line, &function, &mut self.uses);
        for a in &node.attrs {
            check_cfg(a);
        }
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        for a in &node.attrs {
            check_cfg(a);
        }
        self.record_macro(
            path_segments(&node.mac.path),
            node.ident.as_ref().map(ToString::to_string),
            &node.mac.tokens,
            node.span().start().line,
        );
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.record_macro(
            path_segments(&node.path),
            None,
            &node.tokens,
            node.span().start().line,
        );
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        self.stmts.push(compact(&node.to_token_stream()));
        syn::visit::visit_stmt(self, node);
        self.stmts.pop();
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        let binding = Let {
            idents: idents_of(&node.pat.to_token_stream()),
            init: node
                .init
                .as_ref()
                .map(|i| compact(&i.expr.to_token_stream())),
            line: node.span().start().line,
        };
        if let Some(frame) = self.frames.last_mut() {
            frame.lets.push(binding);
        }
        syn::visit::visit_local(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        let callee = unparen(&node.func);
        let (qself, path) = match callee {
            syn::Expr::Path(p) => (
                p.qself.as_ref().map(|q| type_ref(&q.ty)),
                path_segments(&p.path),
            ),
            _ => (None, Vec::new()),
        };
        self.calls.push(Call {
            qself,
            path,
            callee: compact(&callee.to_token_stream()),
            args: node
                .args
                .iter()
                .map(|a| compact(&a.to_token_stream()))
                .collect(),
            stmt: self.stmt(),
            line: node.span().start().line,
            function: self.function(),
        });
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let receiver = unparen(&node.receiver);
        self.method_calls.push(MethodCall {
            method: node.method.to_string(),
            receiver: compact(&receiver.to_token_stream()),
            receiver_method: match receiver {
                syn::Expr::MethodCall(m) => Some(m.method.to_string()),
                _ => None,
            },
            args: node
                .args
                .iter()
                .map(|a| compact(&a.to_token_stream()))
                .collect(),
            stmt: self.stmt(),
            line: node.span().start().line,
            function: self.function(),
        });
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.string_literals.push(StringLit {
            value: node.value(),
            line: node.span().start().line,
            function: self.function(),
            in_macro: false,
        });
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        self.paths.push(PathRef {
            segments: path_segments(node),
            line: node.span().start().line,
            function: self.function(),
            in_macro: false,
        });
        syn::visit::visit_path(self, node);
    }
}

fn flatten_use(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    line: usize,
    function: &Option<String>,
    out: &mut Vec<UseDecl>,
) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            flatten_use(&p.tree, prefix, line, function, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let mut path = prefix.clone();
            path.push(n.ident.to_string());
            out.push(UseDecl {
                path,
                rename: None,
                glob: false,
                line,
                function: function.clone(),
            });
        }
        syn::UseTree::Rename(r) => {
            let mut path = prefix.clone();
            path.push(r.ident.to_string());
            out.push(UseDecl {
                path,
                rename: Some(r.rename.to_string()),
                glob: false,
                line,
                function: function.clone(),
            });
        }
        syn::UseTree::Glob(_) => out.push(UseDecl {
            path: prefix.clone(),
            rename: None,
            glob: true,
            line,
            function: function.clone(),
        }),
        syn::UseTree::Group(g) => {
            for item in &g.items {
                flatten_use(item, prefix, line, function, out);
            }
        }
    }
}
