//! Token-level views: the one compact printing every comparison uses,
//! and what can be read from a macro's tokens, which syn does not parse.

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// The tokens printed with no separators: identifiers, punctuation and
/// literal text concatenated verbatim, groups with their delimiters. A
/// literal keeps its own inner spaces (`"upload: {e}"` compares as
/// written); everything between tokens is gone, so a call wrapped over
/// several lines compacts to the same text as one written on one.
pub fn compact(tokens: &TokenStream) -> String {
    let mut out = String::new();
    push_compact(tokens, &mut out);
    out
}

fn push_compact(tokens: &TokenStream, out: &mut String) {
    for tree in tokens.clone() {
        match tree {
            TokenTree::Ident(ident) => out.push_str(&ident.to_string()),
            TokenTree::Punct(punct) => out.push(punct.as_char()),
            TokenTree::Literal(lit) => out.push_str(&lit.to_string()),
            TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(open);
                push_compact(&group.stream(), out);
                out.push_str(close);
            }
        }
    }
}

/// A string literal found among tokens, with its value (escapes
/// resolved, raw strings verbatim) and line.
#[derive(Debug, Clone)]
pub struct TokenLit {
    pub value: String,
    pub line: usize,
}

/// Every `"…"` / `r"…"` literal in a token stream, groups included, in
/// order. Byte strings, characters and numbers are not strings.
pub fn string_literals_in(tokens: &TokenStream) -> Vec<TokenLit> {
    let mut out = Vec::new();
    push_literals(tokens, &mut out);
    out
}

fn push_literals(tokens: &TokenStream, out: &mut Vec<TokenLit>) {
    for tree in tokens.clone() {
        match tree {
            TokenTree::Literal(lit) => {
                if let syn::Lit::Str(s) = syn::Lit::new(lit.clone()) {
                    out.push(TokenLit {
                        value: s.value(),
                        line: lit.span().start().line,
                    });
                }
            }
            TokenTree::Group(group) => push_literals(&group.stream(), out),
            _ => {}
        }
    }
}

/// A path written in tokens: an identifier followed by one or more
/// `::identifier`, with the line of its first segment.
#[derive(Debug, Clone)]
pub struct TokenPath {
    pub segments: Vec<String>,
    pub line: usize,
}

/// Every `a::b(::c)*` run in a token stream, groups included: what a
/// macro invocation names, since syn leaves its body as tokens.
pub fn paths_in(tokens: &TokenStream) -> Vec<TokenPath> {
    let mut out = Vec::new();
    push_paths(tokens, &mut out);
    out
}

fn push_paths(tokens: &TokenStream, out: &mut Vec<TokenPath>) {
    let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
    let mut i = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Group(group) => {
                push_paths(&group.stream(), out);
                i += 1;
            }
            TokenTree::Ident(first) => {
                let mut segments = vec![first.to_string()];
                let line = first.span().start().line;
                let mut j = i + 1;
                while let (
                    Some(TokenTree::Punct(a)),
                    Some(TokenTree::Punct(b)),
                    Some(TokenTree::Ident(next)),
                ) = (trees.get(j), trees.get(j + 1), trees.get(j + 2))
                {
                    if a.as_char() != ':' || b.as_char() != ':' {
                        break;
                    }
                    segments.push(next.to_string());
                    j += 3;
                }
                if segments.len() > 1 {
                    out.push(TokenPath { segments, line });
                }
                i = j.max(i + 1);
            }
            _ => i += 1,
        }
    }
}

/// An identifier and its line.
#[derive(Debug, Clone)]
pub struct TokenIdent {
    pub name: String,
    pub line: usize,
}

/// Every identifier in a token stream, groups included, in order.
pub fn idents_in(tokens: &TokenStream) -> Vec<TokenIdent> {
    let mut out = Vec::new();
    push_idents(tokens, &mut out);
    out
}

fn push_idents(tokens: &TokenStream, out: &mut Vec<TokenIdent>) {
    for tree in tokens.clone() {
        match tree {
            TokenTree::Ident(ident) => out.push(TokenIdent {
                name: ident.to_string(),
                line: ident.span().start().line,
            }),
            TokenTree::Group(group) => push_idents(&group.stream(), out),
            _ => {}
        }
    }
}
