//! The per-user SQL scan: every SQL literal in a store crate that touches
//! a per-user table must bind `user_id`, or its function must be listed
//! as unscoped by design with a reason. Together with the unforgeable
//! `UserId`, this is what makes "one user reads another's rows" a build
//! failure rather than a code-review hope: the id cannot be invented,
//! and the query cannot forget it.
//!
//! The literals come from the model (plain literals and the ones inside
//! `format!`, `params!` and other macros alike), each attributed to the
//! function it sits in; the reading of the SQL itself is textual, since
//! SQL is text.

use std::path::Path;

use crate::{crate_at, relative};

/// One SQL literal that reads or writes a per-user table.
#[derive(Debug, Clone)]
pub struct Statement {
    /// `file:function` where the literal appears.
    pub site: String,
    pub function: String,
    /// The tables from the per-user list that the statement touches.
    pub tables: Vec<String>,
    /// The literal, whitespace-compacted, for the report.
    pub sql: String,
    pub scoped: bool,
}

impl Statement {
    pub fn line(&self) -> String {
        let sql: String = self.sql.chars().take(90).collect();
        format!("{}  [{}]  {sql}", self.site, self.tables.join(","))
    }
}

/// Every per-user statement under `<crate_dir>/src` (or the given
/// subdirectory), with its scoping verdict. Migrations are skipped: they
/// define tables rather than read them per user.
pub fn statements(crate_dir: &Path, src_subdir: &str, per_user_tables: &[&str]) -> Vec<Statement> {
    statements_excluding(crate_dir, src_subdir, &[], per_user_tables)
}

/// As `statements`, skipping the relative paths in `exclude` (a
/// directory prefix or a file): for a tree that speaks another
/// database's SQL under some of its paths.
pub fn statements_excluding(
    crate_dir: &Path,
    src_subdir: &str,
    exclude: &[&str],
    per_user_tables: &[&str],
) -> Vec<Statement> {
    let krate = crate_at(crate_dir);
    let root = relative(&krate.dir, &krate.dir.join(src_subdir));
    let mut out = Vec::new();
    for file in krate.source_files() {
        let rel = &file.rel;
        if !(rel == &root || rel.starts_with(&format!("{root}/"))) {
            continue;
        }
        if rel.contains("migrations") || rel.starts_with("src/testing") {
            continue;
        }
        if exclude.iter().any(|e| rel.starts_with(e)) {
            continue;
        }
        for literal in &file.string_literals {
            let sql = normalize(&literal.value);
            let upper = sql.to_ascii_uppercase();
            if !is_sql(&upper) {
                continue;
            }
            let tables: Vec<String> = tables_in(&upper)
                .into_iter()
                .filter(|t| per_user_tables.iter().any(|p| p.eq_ignore_ascii_case(t)))
                .map(|t| t.to_ascii_lowercase())
                .collect();
            if tables.is_empty() {
                continue;
            }
            let function = literal.function.clone().unwrap_or_else(|| "?".to_string());
            out.push(Statement {
                site: format!("{rel}:{function}"),
                function,
                tables,
                scoped: is_scoped(&upper),
                sql,
            });
        }
    }
    out
}

/// Runs of whitespace collapsed to one space, so keywords and identifiers
/// keep their boundaries whatever the literal's indentation; SQL comments
/// dropped, so a literal that opens with one is still seen as SQL.
pub fn normalize(literal: &str) -> String {
    let mut out = String::new();
    let mut rest = literal;
    loop {
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix("--") {
            rest = after.find('\n').map(|n| &after[n..]).unwrap_or("");
        } else if let Some(after) = trimmed.strip_prefix("/*") {
            rest = after.find("*/").map(|n| &after[n + 2..]).unwrap_or("");
        } else {
            break;
        }
    }
    for line in rest.lines() {
        let code = line.split("--").next().unwrap_or("");
        out.push_str(code);
        out.push(' ');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_sql(upper: &str) -> bool {
    upper.starts_with("SELECT")
        || upper.starts_with("UPDATE")
        || upper.starts_with("INSERT")
        || upper.starts_with("DELETE")
        || upper.starts_with("WITH")
        || upper.starts_with("REPLACE")
}

/// Identifiers that follow a table-introducing keyword.
pub fn tables_in(upper: &str) -> Vec<String> {
    let mut tables = Vec::new();
    for keyword in ["FROM ", "JOIN ", "UPDATE ", "INTO "] {
        let mut from = 0;
        while let Some(pos) = upper[from..].find(keyword) {
            let at = from + pos + keyword.len();
            from = at;
            // Whole word: not the tail of another identifier.
            let before = upper[..at - keyword.len()].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let mut rest = &upper[at..];
            if rest.starts_with('(') {
                continue; // FROM (subquery)
            }
            // `UPDATE OR REPLACE t`, `INSERT OR IGNORE INTO t`: the
            // conflict clause is not the table.
            for clause in [
                "OR IGNORE ",
                "OR REPLACE ",
                "OR ABORT ",
                "OR FAIL ",
                "OR ROLLBACK ",
            ] {
                if let Some(after) = rest.strip_prefix(clause) {
                    rest = after.trim_start();
                }
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                tables.push(name);
            }
        }
    }
    tables.sort();
    tables.dedup();
    tables
}

/// A statement is scoped when the caller's user is bound into it: an
/// INSERT names `user_id` among its columns (or selects under a bound
/// user), and every other statement compares `user_id` to a parameter
/// (`user_id = ?N`), directly or inside an EXISTS on the owning table.
/// A join between two tables' `user_id` columns is not a scope; only a
/// bound parameter is.
pub fn is_scoped(upper: &str) -> bool {
    if upper.starts_with("INSERT") || upper.starts_with("REPLACE") {
        return upper.contains("USER_ID");
    }
    upper.contains("USER_ID = ?")
}

/// The statements that are neither scoped nor listed, and the list
/// entries that name no function in the scan (so the list cannot rot).
pub fn violations(
    statements: &[Statement],
    unscoped_by_design: &[(&str, &str)],
) -> (Vec<String>, Vec<String>) {
    let unscoped: Vec<String> = statements
        .iter()
        .filter(|s| !s.scoped && !unscoped_by_design.iter().any(|(f, _)| *f == s.function))
        .map(Statement::line)
        .collect();
    let stale: Vec<String> = unscoped_by_design
        .iter()
        .filter(|(f, _)| !statements.iter().any(|s| !s.scoped && s.function == *f))
        .map(|(f, why)| format!("{f} ({why})"))
        .collect();
    (unscoped, stale)
}
