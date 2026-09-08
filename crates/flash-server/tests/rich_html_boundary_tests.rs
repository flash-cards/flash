//! Rich HTML reaches a store column only as `SanitizedHtml`, whose sole
//! constructors are the sanitizers in `flash_store::richtext`. The type
//! is what makes "a rewriter stored unsanitized markup" fail to compile;
//! this scan is what keeps the type on the boundary. Every SQL literal
//! in any crate that writes a rich column must live in the store crate's
//! repo tree, in a function whose signature carries the typed value, so
//! a new write path cannot take a `String` and a new crate cannot write
//! the column at all.

use flash_scan::{workspace_crates, FnKind};

/// A statement that writes rich HTML: an INSERT or UPDATE naming a rich
/// column with a value (setting it to NULL is the plain-card path).
fn writes_rich_html(sql: &str) -> bool {
    let compact: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let upper = compact.to_ascii_uppercase();
    let is_write = ["INSERT", "UPDATE", "REPLACE", "WITH"]
        .iter()
        .any(|k| upper.starts_with(k));
    let names_column = upper.contains("FRONT_HTML") || upper.contains("BACK_HTML");
    let only_nulls = upper.contains("FRONT_HTML = NULL") && upper.contains("BACK_HTML = NULL");
    is_write && names_column && !only_nulls
}

/// The types that carry `SanitizedHtml` into a store function.
const TYPED: &[&str] = &["SanitizedHtml", "CardExtras", "NoteSave"];

#[test]
fn rich_columns_are_written_only_by_typed_store_functions() {
    let mut writers = Vec::new();
    let mut offenders = Vec::new();
    for krate in workspace_crates() {
        for file in krate.source_files() {
            for function in &file.functions {
                let writes = file.string_literals.iter().any(|l| {
                    l.function.as_deref() == Some(&function.name) && writes_rich_html(&l.value)
                });
                if !writes {
                    continue;
                }
                let site = format!("{}/{}:{}", krate.name, file.rel, function.name);
                writers.push(site.clone());
                let in_store_repo =
                    krate.name == "flash-store" && file.rel.starts_with("src/repo/");
                let typed = function
                    .signature_idents()
                    .iter()
                    .any(|i| TYPED.contains(&i.as_str()));
                if !in_store_repo {
                    offenders.push(format!(
                        "{site}: rich column written outside flash-store/src/repo"
                    ));
                } else if !typed {
                    offenders.push(format!(
                        "{site}: signature does not take SanitizedHtml, CardExtras or NoteSave"
                    ));
                }
            }
        }
    }
    assert!(
        writers.len() >= 4,
        "the scan found too few rich-column writers to be trusted:\n{}",
        writers.join("\n")
    );
    assert!(
        offenders.is_empty(),
        "rich HTML written past the type boundary:\n{}",
        offenders.join("\n")
    );
}

/// The store's row mappers read the columns back as plain strings (a
/// page renders them with `|safe`); nothing else in the tree may build
/// the type by hand. The field is private, so this is a compile-time
/// fact; the test states it where the scan lives, and refuses a call
/// hidden inside a macro rather than reading past it.
#[test]
fn the_type_has_no_constructor_outside_the_sanitizer_module() {
    let mut offenders = Vec::new();
    for krate in workspace_crates() {
        for file in krate.source_files() {
            if krate.name == "flash-store" && file.rel == "src/richtext.rs" {
                continue;
            }
            for call in file
                .calls
                .iter()
                .filter(|c| c.ends_with(&["SanitizedHtml"]))
            {
                offenders.push(format!("{}/{}:{}", krate.name, file.rel, call.line));
            }
            for p in file.paths.iter().filter(|p| p.in_macro) {
                if p.ends_with(&["SanitizedHtml"]) {
                    offenders.push(format!(
                        "{}/{}:{}: SanitizedHtml named inside a macro",
                        krate.name, file.rel, p.line
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "SanitizedHtml built outside richtext.rs:\n{}",
        offenders.join("\n")
    );
}

/// Inside the sanitizer module itself, the type is built in exactly the
/// two sanitizers and the trim: a rewriter in that file ending in
/// `SanitizedHtml(out)` would otherwise be a constructor the type's
/// privacy cannot refuse.
#[test]
fn inside_the_sanitizer_module_only_the_sanitizers_build_the_type() {
    const CONSTRUCTORS: &[&str] = &["trimmed", "sanitize_card_html", "sanitize_with_media"];
    let store = workspace_crates()
        .into_iter()
        .find(|k| k.name == "flash-store")
        .expect("flash-store");
    let file = store.file("src/richtext.rs");
    let mut seen = Vec::new();
    let mut offenders = Vec::new();
    for call in &file.calls {
        let builds = call.path == ["SanitizedHtml"]
            || (call.path == ["Self"]
                && call.function.as_deref().and_then(|n| file.function(n)).is_some_and(|f| {
                    matches!(&f.kind, FnKind::Method { owner, .. } if owner == "SanitizedHtml")
                }));
        if !builds {
            continue;
        }
        match call.function.as_deref() {
            Some(name) if CONSTRUCTORS.contains(&name) => seen.push(name.to_string()),
            Some(name) => offenders.push(format!("{name} (line {})", call.line)),
            None => offenders.push(format!("outside any function (line {})", call.line)),
        }
    }
    assert!(
        offenders.is_empty(),
        "functions in richtext.rs that build SanitizedHtml besides the sanitizers: {offenders:?}"
    );
    for c in CONSTRUCTORS {
        assert!(seen.iter().any(|s| s == c), "{c} no longer builds the type");
    }
}
