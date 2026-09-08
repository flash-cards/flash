//! The route inventory: every `.route("…", method(…))` declared in a set
//! of crate source trees, with the prefix it is mounted under. The tests
//! built on it are exhaustive by construction: a route added anywhere in
//! the scanned crates is probed on the next run, and it either turns a
//! stranger away or has to be named on a public list with a reason.
//!
//! Source scanning is deliberate. axum's `Router` keeps no readable table
//! of its routes, and an inventory a test writes by hand cannot notice
//! the route it forgot. What the scanner cannot read, it refuses: a path
//! that is not a literal or a same-file constant, a handler chain with no
//! method constructor, a route declared inside a macro, a nested router
//! it does not know how to expand.

use std::collections::BTreeSet;
use std::path::Path;

use quote::ToTokens;

use crate::tokens::compact;
use crate::{crate_at, MethodCall, SourceFile};

/// One mounted route, as an HTTP method and a path that still carries
/// its `{param}` placeholders.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Route {
    pub method: String,
    pub path: String,
    /// `file:function` of the declaration, for the failure message.
    pub site: String,
}

impl Route {
    pub fn is_mutation(&self) -> bool {
        !matches!(self.method.as_str(), "GET" | "HEAD" | "OPTIONS")
    }

    pub fn is_api(&self) -> bool {
        self.path.starts_with("/api/v1/")
    }
}

const METHODS: [&str; 7] = ["get", "post", "put", "patch", "delete", "head", "any"];

/// Every route declared under `<crate_dir>/src` for each crate given,
/// mounted under `/api/v1` when it is declared in the `api` module tree,
/// in an `api_*.rs` file, or in a function whose name starts with `api`
/// (the two conventions the extension trait's `api_routes` gathers from).
/// Binaries (`main.rs`) are skipped: they compose the library router and
/// add only the health probe. The MCP endpoint is a nested service, not
/// a route, so it is added by hand when the scanned tree mounts it.
pub fn inventory(crate_dirs: &[&Path]) -> Vec<Route> {
    let mut routes = BTreeSet::new();
    for dir in crate_dirs {
        for file in crate_at(dir).source_files() {
            if file.rel.ends_with("/main.rs") || file.rel.starts_with("src/testing") {
                continue;
            }
            scan_file(&file, &mut routes);
        }
    }
    routes.into_iter().collect()
}

fn scan_file(file: &SourceFile, out: &mut BTreeSet<Route>) {
    let file_is_api = file.rel.contains("/api/") || {
        let stem = file.rel.rsplit('/').next().unwrap_or("");
        stem.starts_with("api_")
    };
    for m in &file.macros {
        if compact(&m.tokens).contains(".route(") {
            panic!(
                "{}:{}: a route declared inside a macro; the inventory cannot read it",
                file.rel, m.line
            );
        }
    }
    for call in &file.method_calls {
        let site = format!("{}:{}", file.rel, call.function.as_deref().unwrap_or("?"));
        match call.method.as_str() {
            "nest_service" => {
                assert!(
                    call.args.first().map(String::as_str) == Some("\"/mcp\""),
                    "{site}: a nested service the inventory cannot expand: {}",
                    call.args.join(",")
                );
                for method in ["GET", "POST", "DELETE"] {
                    out.insert(Route {
                        method: method.to_string(),
                        path: "/mcp".to_string(),
                        site: format!("{}:nest_service", file.rel),
                    });
                }
            }
            "nest" => assert!(
                call.args.first().map(String::as_str) == Some("\"/api/v1\""),
                "{site}: a nested router the inventory cannot expand: {}",
                call.args.join(",")
            ),
            "route_service" => panic!("{site}: a route service the inventory cannot read"),
            "route" => {
                let is_api = file_is_api
                    || call
                        .function
                        .as_deref()
                        .is_some_and(|f| f.starts_with("api"));
                scan_route(file, call, &site, is_api, out);
            }
            _ => {}
        }
    }
}

fn scan_route(
    file: &SourceFile,
    call: &MethodCall,
    site: &str,
    is_api: bool,
    out: &mut BTreeSet<Route>,
) {
    assert!(
        call.args.len() == 2,
        "{site}: a route with {} arguments",
        call.args.len()
    );
    let path = match parse_expr(&call.args[0]) {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => s.value(),
        syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            let name = p.path.segments[0].ident.to_string();
            file.consts.get(&name).cloned().unwrap_or_else(|| {
                panic!("{site}: route path `{name}` is neither a literal nor a same-file const")
            })
        }
        _ => panic!(
            "{site}: route path `{}` is neither a literal nor a same-file const",
            call.args[0]
        ),
    };
    let mounted = if is_api {
        format!("/api/v1{path}")
    } else {
        path
    };
    let methods = spine_methods(&parse_expr(&call.args[1]));
    assert!(
        !methods.is_empty(),
        "{site}: route `{mounted}` names no method constructor the scanner knows ({})",
        call.args[1]
    );
    for method in methods {
        out.insert(Route {
            method: method.to_uppercase(),
            path: mounted.clone(),
            site: site.to_string(),
        });
    }
}

fn parse_expr(compact_expr: &str) -> syn::Expr {
    syn::parse_str(compact_expr)
        .unwrap_or_else(|e| panic!("unreadable expression `{compact_expr}`: {e}"))
}

/// The method constructors along the spine of a handler expression:
/// `get(a).post(b)` yields both, `on(MethodFilter::PUT, h)` yields put,
/// `get_service(s)` yields get; `.layer(…)` and `.with_state(…)` are
/// passed over, and arguments are never looked into.
fn spine_methods(expr: &syn::Expr) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = expr;
    loop {
        match current {
            syn::Expr::Paren(p) => current = &p.expr,
            syn::Expr::MethodCall(m) => {
                if let Some(method) = constructor(&m.method.to_string(), &m.args) {
                    push_unique(&mut found, method);
                }
                current = &m.receiver;
            }
            syn::Expr::Call(c) => {
                if let syn::Expr::Path(p) = &*c.func {
                    let last = p
                        .path
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default();
                    if let Some(method) = constructor(&last, &c.args) {
                        push_unique(&mut found, method);
                    }
                }
                break;
            }
            _ => break,
        }
    }
    found.reverse();
    found
}

fn constructor(
    name: &str,
    args: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
) -> Option<String> {
    if METHODS.contains(&name) {
        return Some(name.to_string());
    }
    if let Some(base) = name.strip_suffix("_service") {
        if METHODS.contains(&base) {
            return Some(base.to_string());
        }
    }
    if name == "on" {
        if let Some(syn::Expr::Path(p)) = args.first() {
            let segments: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if segments.len() >= 2 && segments[segments.len() - 2] == "MethodFilter" {
                return Some(segments[segments.len() - 1].to_lowercase());
            }
        }
        panic!(
            "a route registered with `on(…)` whose filter the scanner cannot read: {}",
            compact(&args.to_token_stream())
        );
    }
    None
}

fn push_unique(found: &mut Vec<String>, method: String) {
    if !found.contains(&method) {
        found.push(method);
    }
}

/// Every `file:line ident` under `<crate_dir>/src` in files whose
/// relative path starts with one of `prefixes`, where the shipped tokens
/// name one of `identifiers` as a whole word (macro bodies included).
/// The API-band tests use it to keep cookie-accepting extractors out of
/// the API's handler files.
pub fn identifier_sites(crate_dir: &Path, prefixes: &[&str], identifiers: &[&str]) -> Vec<String> {
    let mut sites = Vec::new();
    for file in crate_at(crate_dir).source_files() {
        if !prefixes.iter().any(|p| file.rel.starts_with(p)) {
            continue;
        }
        for ident in &file.idents {
            if identifiers.contains(&ident.name.as_str()) {
                sites.push(format!("{}:{} {}", file.rel, ident.line, ident.name));
            }
        }
    }
    sites
}
