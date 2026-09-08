//! `UserId` cannot be built from an integer in ordinary code (its field is
//! private); the two named constructors are the whole escape hatch. This
//! test pins where each may appear, so a new call site in a handler is a
//! failing test rather than a code-review hope.
//!
//! - `UserId::from_db` belongs to the store crates' row mappers, and
//!   takes a column read from a row.
//! - `UserId::assume_authorized` belongs to the admin routes, which act
//!   on a user an administrator chose.

use flash_scan::{workspace_crates, Call, Function, SourceFile};

/// Every shipped file in every crate except the type's own module.
fn files() -> Vec<(String, SourceFile)> {
    let mut out = Vec::new();
    for krate in workspace_crates() {
        for file in krate.source_files() {
            if krate.name == "flash-core" && file.rel == "src/ids.rs" {
                continue;
            }
            out.push((format!("crates/{}/{}", krate.name, file.rel), file));
        }
    }
    out
}

/// The argument is a column read from a row, or the row id SQLite just
/// assigned: in the argument itself, in the statement around the call
/// (a closure mapping a row), or in the `let` that bound the name.
fn from_a_row(call: &Call, function: Option<&Function>) -> bool {
    const READS: &[&str] = &[".get(", ".get::<", "last_insert_rowid("];
    let reads = |text: &str| READS.iter().any(|r| text.contains(r));
    let Some(arg) = call.args.first() else {
        return false;
    };
    if reads(arg) || reads(&call.stmt) {
        return true;
    }
    function.is_some_and(|f| {
        f.lets
            .iter()
            .any(|l| l.idents.iter().any(|i| i == arg) && l.init.as_deref().is_some_and(reads))
    })
}

#[test]
fn from_db_is_called_only_by_row_mappers_on_a_row_column() {
    let mut stray = Vec::new();
    let mut not_from_a_row = Vec::new();
    let mut aliased = Vec::new();
    let mut sites = 0;
    for (path, file) in files() {
        // Aliasing the type would hide the call from the scan.
        for u in &file.uses {
            if u.path.last().is_some_and(|l| l == "UserId") && u.rename.is_some() {
                aliased.push(format!(
                    "{path}:{}: use … as {}",
                    u.line,
                    u.rename.clone().unwrap()
                ));
            }
        }
        for p in file.paths.iter().filter(|p| p.in_macro) {
            if p.ends_with(&["UserId", "from_db"]) {
                aliased.push(format!("{path}:{}: from_db inside a macro", p.line));
            }
        }
        for call in file
            .calls
            .iter()
            .filter(|c| c.ends_with(&["UserId", "from_db"]))
        {
            sites += 1;
            let site = format!("{path}:{}", call.line);
            // A row mapper lives in a crate's `src/repo/` or `src/store/`
            // tree; that is where a database row becomes a typed id.
            if !(file.rel.starts_with("src/repo/") || file.rel.starts_with("src/store/")) {
                stray.push(site.clone());
            }
            let function = call.function.as_deref().and_then(|n| file.function(n));
            if !from_a_row(call, function) {
                not_from_a_row.push(format!("{site}: {}({})", call.callee, call.args.join(",")));
            }
        }
    }
    // The open store alone has eight; a broken scan finds none.
    assert!(sites >= 5, "the scan found only {sites} from_db call sites");
    assert!(
        stray.is_empty(),
        "UserId::from_db outside the store's row mappers:\n{}",
        stray.join("\n")
    );
    assert!(
        not_from_a_row.is_empty(),
        "UserId::from_db with something other than a row column:\n{}",
        not_from_a_row.join("\n")
    );
    assert!(
        aliased.is_empty(),
        "an alias of UserId or a call the scan cannot follow:\n{}",
        aliased.join("\n")
    );
}

/// Every function, in any crate of the workspace, that assumes an id
/// must take an admin extractor: the id it names was chosen by an
/// administrator, and the signature is where that is proven. The open
/// core has no such function today; a downstream extension's admin
/// routes are checked the same way when they are present, so the rule
/// holds for whatever is in the tree.
#[test]
fn assume_authorized_appears_only_in_functions_that_take_an_admin_extractor() {
    let mut offenders = Vec::new();
    for (path, file) in files() {
        for call in file
            .calls
            .iter()
            .filter(|c| c.ends_with(&["UserId", "assume_authorized"]))
        {
            let Some(function) = call.function.as_deref().and_then(|n| file.function(n)) else {
                offenders.push(format!("{path}:{}: outside any function", call.line));
                continue;
            };
            // Either extractor, destructured (`AdminUser(admin)`) or typed
            // (`_admin: BearerAdmin`), in the parameter list.
            let admin = function.params.iter().any(|p| {
                ["AdminUser", "BearerAdmin"].iter().any(|e| {
                    p.pat_idents.iter().any(|i| i == e) || p.ty.idents.iter().any(|i| i == e)
                })
            });
            if !admin {
                offenders.push(format!("{path}:{}", function.name));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "UserId::assume_authorized in a function without an admin extractor:\n{}",
        offenders.join("\n")
    );
}
