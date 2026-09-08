//! Log lines never carry a user-controlled string through `Display`: a
//! filename, slug, report reason, path, token or address goes through
//! `Debug` (which escapes newlines and control characters) or not at
//! all, so a crafted value cannot forge a second log line or smuggle a
//! terminal escape into whoever reads the journal.

use flash_scan::proc_macro2::TokenTree;
use flash_scan::{parse_source, string_literals_in, workspace_crates, MacroCall};

/// Identifiers whose value a stranger can choose.
const SENSITIVE: &[&str] = &[
    "name",
    "filename",
    "file",
    "slug",
    "reason",
    "path",
    "token",
    "purchase_token",
    "email",
    "subject",
    "q",
    "query",
    "label",
    "client_name",
    "redirect_uri",
    "message",
    "user_agent",
    "referrer",
];

const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// A logging macro, written bare or as `tracing::level!`.
fn is_log(m: &MacroCall) -> bool {
    match m.path.as_slice() {
        [level] => LEVELS.contains(&level.as_str()),
        [krate, level] => krate == "tracing" && LEVELS.contains(&level.as_str()),
        _ => false,
    }
}

/// The sensitive identifiers a log invocation prints with Display: the
/// `%ident` (and `%ident.field`) sigil, and `{ident}` or `{ident:<8}`
/// (a format spec that is not `?`) inside its message literal.
fn display_uses(call: &MacroCall) -> Vec<String> {
    let mut hits = Vec::new();
    let tokens: Vec<TokenTree> = call.tokens.clone().into_iter().collect();
    for (i, tree) in tokens.iter().enumerate() {
        let TokenTree::Punct(p) = tree else { continue };
        if p.as_char() != '%' {
            continue;
        }
        // The sigil's operand: `name`, or a field path `entry.filename`.
        let mut j = i + 1;
        while let Some(TokenTree::Ident(ident)) = tokens.get(j) {
            let name = ident.to_string();
            if SENSITIVE.contains(&name.as_str()) {
                hits.push(name);
            }
            match tokens.get(j + 1) {
                Some(TokenTree::Punct(dot)) if dot.as_char() == '.' => j += 2,
                _ => break,
            }
        }
    }
    for lit in string_literals_in(&call.tokens) {
        for ident in SENSITIVE {
            let mut from = 0;
            while let Some(pos) = lit.value[from..].find(&format!("{{{ident}")) {
                let after = &lit.value[from + pos + ident.len() + 1..];
                if after.starts_with('}') || (after.starts_with(':') && !after.starts_with(":?")) {
                    hits.push(ident.to_string());
                }
                from += pos + 1;
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

/// Files whose only match is a value the server minted itself.
const ALLOWED: &[(&str, &str)] = &[(
    "src/boot.rs",
    "the bootstrap invite link is generated here for the operator's journal; nobody else chooses it",
)];

#[test]
fn user_controlled_strings_are_logged_with_debug_not_display() {
    let mut offenders = Vec::new();
    let mut seen = 0;
    for krate in workspace_crates() {
        for file in krate.source_files() {
            if ALLOWED.iter().any(|(path, _)| *path == file.rel) {
                continue;
            }
            for call in file.macros.iter().filter(|m| is_log(m)) {
                seen += 1;
                let hits = display_uses(call);
                if !hits.is_empty() {
                    offenders.push(format!(
                        "{}/{}:{} logs {} with Display",
                        krate.name,
                        file.rel,
                        call.line,
                        hits.join(", ")
                    ));
                }
            }
        }
    }
    assert!(seen > 50, "the scan found only {seen} log invocations");
    assert!(
        offenders.is_empty(),
        "log lines that print a user-controlled value with Display (use `?`):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_reads_both_interpolation_styles_bare_or_qualified() {
    let file = parse_source(
        "snippet.rs",
        r#"
        fn f() {
            tracing::warn!(user = 1, file = %name, "upload failed: {e}");
            info!("share {slug} reported: {reason:?}");
            tracing::info!(file = ?name, "fine {label:<12}");
            tracing::debug!(%entry.filename, "seen");
        }
        "#,
    );
    let calls: Vec<&MacroCall> = file.macros.iter().filter(|m| is_log(m)).collect();
    assert_eq!(calls.len(), 4);
    assert_eq!(display_uses(calls[0]), vec!["name"]);
    assert_eq!(display_uses(calls[1]), vec!["slug"]);
    assert_eq!(display_uses(calls[2]), vec!["label"]);
    assert_eq!(display_uses(calls[3]), vec!["filename"]);
}
