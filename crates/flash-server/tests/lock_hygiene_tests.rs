//! No lock in the process can be poisoned, and no request can drop its
//! connection by panicking. The release profile unwinds on panic so a
//! bad request costs one request; that promise holds only if a panic
//! taken while a lock is held leaves the lock usable. A `std::sync`
//! mutex does not (every later `lock()` panics, and the process stays
//! up looking healthy to its supervisor), so none may exist in shipped
//! source, in any crate of the workspace. `parking_lot` and `tokio::sync`
//! locks have no poisoned state and are what the tree uses.
//!
//! The scan is exhaustive by construction: whatever ships in `src/` of
//! every crate is what gets checked, by the paths written and the calls
//! made rather than by text, so an alias or a glob import changes
//! nothing. `clippy.toml` bans the same types by their resolved names.

use flash_scan::{crate_at, workspace_crates};

const POISONABLE: &[&str] = &[
    "Mutex",
    "RwLock",
    "MutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
];

#[test]
fn no_poisonable_lock_exists_in_any_crate() {
    let mut offenders = Vec::new();
    for krate in workspace_crates() {
        for file in krate.source_files() {
            let rel = format!("{}/{}", krate.name, file.rel);
            for u in &file.uses {
                let std_sync = u.path.starts_with(&["std".to_string(), "sync".to_string()]);
                let names_lock = u
                    .path
                    .last()
                    .is_some_and(|l| POISONABLE.contains(&l.as_str()));
                if std_sync && (names_lock || u.glob) {
                    offenders.push(format!(
                        "{rel}:{}: `use {}` (a std lock poisons on panic; use parking_lot)",
                        u.line,
                        u.path.join("::")
                    ));
                }
            }
            for p in &file.paths {
                let std_lock = POISONABLE
                    .iter()
                    .any(|l| p.contains_run(&["std", "sync", l]) || p.contains_run(&["sync", l]));
                if std_lock || p.ends_with(&["PoisonError"]) {
                    offenders.push(format!(
                        "{rel}:{}: `{}` (a std lock poisons on panic; handling poison means one exists)",
                        p.line,
                        p.segments.join("::")
                    ));
                }
            }
            for m in &file.method_calls {
                let on_guard = m
                    .receiver_method
                    .as_deref()
                    .is_some_and(|r| matches!(r, "lock" | "read" | "write"));
                let unwraps = matches!(m.method.as_str(), "unwrap" | "expect" | "unwrap_or_else");
                if on_guard && unwraps {
                    offenders.push(format!(
                        "{rel}:{}: `.{}().{}(` (unwrapping a lock result means the lock can be poisoned)",
                        m.line,
                        m.receiver_method.clone().unwrap_or_default(),
                        m.method
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "poisonable locks in shipped source:\n{}",
        offenders.join("\n")
    );
}

/// The other half of "a panic costs one request": the router catches
/// it and answers 500 instead of dropping the connection.
#[test]
fn the_router_catches_panics() {
    let lib = crate_at(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).file("src/lib.rs");
    let installs = lib.calls.iter().any(|c| {
        c.function.as_deref() == Some("build_app")
            && c.ends_with(&["CatchPanicLayer", "custom"])
            && c.args
                .first()
                .is_some_and(|a| a.ends_with("middleware::panic_response"))
    });
    assert!(installs, "build_app must install the catch-panic layer");
}
