//! The source model behind Flash's structural tests. Every crate in the
//! workspace is parsed with syn, what does not ship is removed, and the
//! rest is offered as lists a test can ask questions of: the functions
//! with their signatures and what they call, the types with their
//! derives and fields, the imports, the macros, the string literals, the
//! paths. A test that once searched text for a pattern asks the model
//! instead, and the model fails closed: a file that does not parse, or
//! a construct it cannot read, is a panic rather than a silent gap.
//!
//! Two scans that every crate shares live here too: the route inventory
//! (`routes`) and the per-user SQL scan (`sql`).

pub mod cfg;
mod extract;
pub mod routes;
pub mod sql;
pub mod tokens;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use proc_macro2;
pub use tokens::{compact, idents_in, paths_in, string_literals_in};

/// One crate directory of the workspace.
#[derive(Debug, Clone)]
pub struct Crate {
    pub name: String,
    pub dir: PathBuf,
}

/// Every `crates/*` directory with a `src`, sorted by name: the open
/// crates alone in the public repository, plus any downstream extension
/// checked out beside them. A scan over this list covers whatever is in
/// the tree.
pub fn workspace_crates() -> Vec<Crate> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out: Vec<Crate> = std::fs::read_dir(&crates)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("src").is_dir())
        .map(|dir| crate_at(&dir))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The crate at `dir`, named by its directory.
pub fn crate_at(dir: &Path) -> Crate {
    let dir = dir
        .canonicalize()
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    Crate {
        name: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        dir,
    }
}

impl Crate {
    /// Every `.rs` under `src`, recursive, sorted, parsed and stripped of
    /// what does not ship. A file that only a `mod x;` under a false
    /// `cfg` names is not shipped either, and is left out.
    pub fn source_files(&self) -> Vec<SourceFile> {
        let mut files = Vec::new();
        walk(&self.dir.join("src"), &mut files);
        files.sort();
        let parsed: Vec<(PathBuf, syn::File)> = files
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                (path.clone(), parse(&path.display().to_string(), &text))
            })
            .collect();
        let excluded = unshipped_modules(&parsed);
        parsed
            .into_iter()
            .filter(|(path, _)| {
                !excluded
                    .iter()
                    .any(|dir| path.starts_with(dir) || path == dir)
            })
            .map(|(path, mut ast)| {
                cfg::strip(&mut ast);
                let rel = relative(&self.dir, &path);
                extract::source_file(path, rel, &ast)
            })
            .collect()
    }

    /// One file by its path relative to the crate (`src/service.rs`).
    pub fn file(&self, rel: &str) -> SourceFile {
        self.source_files()
            .into_iter()
            .find(|f| f.rel == rel)
            .unwrap_or_else(|| panic!("{}: no shipped file {rel}", self.name))
    }
}

/// Parses a snippet standing in for a file (the model's own tests);
/// `label` is reported as its path.
pub fn parse_source(label: &str, text: &str) -> SourceFile {
    let mut ast = parse(label, text);
    cfg::strip(&mut ast);
    extract::source_file(PathBuf::from(label), label.to_string(), &ast)
}

fn parse(label: &str, text: &str) -> syn::File {
    syn::parse_file(text).unwrap_or_else(|e| {
        let at = e.span().start();
        panic!("{label}:{}:{}: {e}", at.line, at.column)
    })
}

/// The directories and files named by an out-of-line `mod x;` that does
/// not ship: `x.rs` and `x/` beside the declaring file (or beside its
/// own directory for a `mod.rs`, `lib.rs` or `main.rs`).
fn unshipped_modules(parsed: &[(PathBuf, syn::File)]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, ast) in parsed {
        let base = match path.file_name().and_then(|n| n.to_str()) {
            Some("mod.rs" | "lib.rs" | "main.rs") => path.parent().map(Path::to_path_buf),
            _ => path
                .parent()
                .map(|p| p.join(path.file_stem().unwrap_or_default())),
        };
        let Some(base) = base else { continue };
        for item in &ast.items {
            if let syn::Item::Mod(m) = item {
                if m.content.is_none() && !cfg::shipped(&m.attrs) {
                    out.push(base.join(format!("{}.rs", m.ident)));
                    out.push(base.join(m.ident.to_string()));
                }
            }
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `path` relative to `root`, with forward slashes.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// One parsed file, as lists over its shipped source. Every entry
/// carries its line and the name of the innermost enclosing function
/// (`None` outside one); closures do not open a scope, a nested `fn`
/// does.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    /// Relative to the crate directory, forward slashes: `src/web.rs`.
    pub rel: String,
    pub functions: Vec<Function>,
    pub types: Vec<TypeDecl>,
    pub uses: Vec<UseDecl>,
    /// `const NAME: &str = "…"` items, by name.
    pub consts: HashMap<String, String>,
    pub calls: Vec<Call>,
    pub method_calls: Vec<MethodCall>,
    pub macros: Vec<MacroCall>,
    pub string_literals: Vec<StringLit>,
    pub paths: Vec<PathRef>,
    /// Every identifier in the file's shipped tokens, macro bodies
    /// included: the whole-word view.
    pub idents: Vec<tokens::TokenIdent>,
}

impl SourceFile {
    /// The first function with this bare name.
    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// The first type declaration with this name.
    pub fn type_named(&self, name: &str) -> Option<&TypeDecl> {
        self.types.iter().find(|t| t.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vis {
    Pub,
    /// `pub(crate)`, `pub(super)`, `pub(in a::b)`: the restriction as written.
    Restricted(String),
    Private,
    /// A trait item, whose visibility is the trait's.
    Inherited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnKind {
    Free,
    /// A method in an `impl` block: the self type (its last path segment)
    /// and, for a trait impl, the trait.
    Method {
        owner: String,
        trait_: Option<String>,
    },
    /// A method declared in a trait, with or without a default body.
    TraitDecl {
        trait_: String,
        has_default: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub kind: FnKind,
    pub vis: Vis,
    pub is_async: bool,
    pub attrs: Vec<Attr>,
    pub has_self: bool,
    pub params: Vec<Param>,
    pub ret: Option<TypeRef>,
    /// `let` bindings in the body, in order.
    pub lets: Vec<Let>,
    /// First and last line of the item.
    pub lines: (usize, usize),
    /// Inline module path, outermost first.
    pub module: Vec<String>,
    sig_idents: Vec<String>,
}

impl Function {
    /// Every identifier in the signature: generics, parameter patterns
    /// and types, return type, where clause.
    pub fn signature_idents(&self) -> &[String] {
        &self.sig_idents
    }

    /// The attribute whose path ends in `name`.
    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs
            .iter()
            .find(|a| a.path.last().is_some_and(|last| last == name))
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    /// The pattern, compact.
    pub pat: String,
    pub pat_idents: Vec<String>,
    pub ty: TypeRef,
}

/// A type as written: its compact text and every identifier in it
/// (path segments and generic arguments alike).
#[derive(Debug, Clone)]
pub struct TypeRef {
    pub compact: String,
    pub idents: Vec<String>,
}

/// An attribute other than a doc comment: its path and, for a list or a
/// value, the compact text of what it carries.
#[derive(Debug, Clone)]
pub struct Attr {
    pub path: Vec<String>,
    pub args: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Let {
    pub idents: Vec<String>,
    /// The initializer, compact.
    pub init: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    Struct { tuple: bool },
    Enum,
    Union,
}

#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
    pub kind: TypeKind,
    pub vis: Vis,
    /// The last segment of every path in every `#[derive(...)]`.
    pub derives: Vec<String>,
    pub attrs: Vec<Attr>,
    pub fields: Vec<Field>,
    pub line: usize,
    pub module: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Field {
    /// `None` for a tuple field.
    pub name: Option<String>,
    /// The enum variant this field belongs to, if any.
    pub variant: Option<String>,
    pub ty: TypeRef,
    pub attrs: Vec<Attr>,
    /// The string of `#[serde(deserialize_with = "…")]`, when present.
    pub deserialize_with: Option<String>,
    pub line: usize,
}

/// One imported name, brace groups flattened: `use a::{b, c as d}` is
/// two entries.
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub path: Vec<String>,
    pub rename: Option<String>,
    pub glob: bool,
    pub line: usize,
    pub function: Option<String>,
}

/// A call whose callee is written as a path, or any other call.
#[derive(Debug, Clone)]
pub struct Call {
    /// `<T>::f`: the qualified self type.
    pub qself: Option<TypeRef>,
    /// The callee's path segments; empty when the callee is not a path.
    pub path: Vec<String>,
    /// The callee, compact.
    pub callee: String,
    /// Each argument, compact.
    pub args: Vec<String>,
    /// The enclosing statement, compact.
    pub stmt: String,
    pub line: usize,
    pub function: Option<String>,
}

impl Call {
    /// The qualified self type's identifiers followed by the path, so
    /// `<UserId>::from_db` and `UserId::from_db` both end in
    /// `["UserId", "from_db"]`.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .qself
            .as_ref()
            .map(|q| q.idents.clone())
            .unwrap_or_default();
        out.extend(self.path.iter().cloned());
        out
    }

    /// Whether the callee path ends in these segments.
    pub fn ends_with(&self, tail: &[&str]) -> bool {
        let names = self.names();
        names.len() >= tail.len()
            && names[names.len() - tail.len()..]
                .iter()
                .zip(tail)
                .all(|(a, b)| a == b)
    }
}

#[derive(Debug, Clone)]
pub struct MethodCall {
    pub method: String,
    /// The receiver, compact.
    pub receiver: String,
    /// The method the receiver itself was produced by, if it was a
    /// method call: `x.lock().unwrap()` has `Some("lock")`.
    pub receiver_method: Option<String>,
    pub args: Vec<String>,
    pub stmt: String,
    pub line: usize,
    pub function: Option<String>,
}

/// A macro invocation, or a `macro_rules!` definition (then `name` is
/// the macro defined and `path` is `["macro_rules"]`).
#[derive(Debug, Clone)]
pub struct MacroCall {
    pub path: Vec<String>,
    pub name: Option<String>,
    pub tokens: proc_macro2::TokenStream,
    pub line: usize,
    pub function: Option<String>,
}

impl MacroCall {
    /// The macro's last path segment.
    pub fn last(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("")
    }
}

#[derive(Debug, Clone)]
pub struct StringLit {
    pub value: String,
    pub line: usize,
    pub function: Option<String>,
    /// Found among a macro's tokens rather than in parsed code.
    pub in_macro: bool,
}

#[derive(Debug, Clone)]
pub struct PathRef {
    pub segments: Vec<String>,
    pub line: usize,
    pub function: Option<String>,
    pub in_macro: bool,
}

impl PathRef {
    pub fn ends_with(&self, tail: &[&str]) -> bool {
        self.segments.len() >= tail.len()
            && self.segments[self.segments.len() - tail.len()..]
                .iter()
                .zip(tail)
                .all(|(a, b)| a == b)
    }

    /// Whether the segments contain this run, in order and adjacent.
    pub fn contains_run(&self, run: &[&str]) -> bool {
        self.segments
            .windows(run.len())
            .any(|w| w.iter().zip(run).all(|(a, b)| a == b))
    }
}
