//! The schema registry: a fresh database records the core's steps and
//! reopens as a no-op; an extension's steps run once, in order, and are
//! recorded under its name; a pre-registry database at the last
//! single-binary schema is adopted, any other pre-registry version is
//! refused.

use std::sync::Arc;

use flash_store::ext::{Migration, StoreExtension};
use flash_store::{Store, MONOLITH_MARKER};
use rusqlite::Connection;

fn temp_db(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("flash-migrations-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

struct Widgets;

const WIDGET_STEPS: &[Migration] = &[
    Migration {
        name: "0001_widgets",
        sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;",
    },
    Migration {
        name: "0002_widget_color",
        sql: "ALTER TABLE widgets ADD COLUMN color TEXT;",
    },
];

impl StoreExtension for Widgets {
    fn name(&self) -> &'static str {
        "widgets"
    }
    fn migrations(&self) -> &'static [Migration] {
        WIDGET_STEPS
    }
}

fn names(store: &Store, extension: &str) -> Vec<String> {
    store
        .applied_migrations()
        .unwrap()
        .into_iter()
        .filter(|(e, _, _)| e == extension)
        .map(|(_, n, _)| n)
        .collect()
}

#[test]
fn fresh_database_records_the_core_steps_and_reopens_quietly() {
    let path = temp_db("fresh");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 15);
    let core = names(&store, "core");
    assert_eq!(core.len(), 15);
    assert_eq!(core[0], "v001_initial");
    assert_eq!(core[13], "v014_reserved");
    assert_eq!(core[14], "v015_review_client");
    assert!(!core.iter().any(|n| n == MONOLITH_MARKER));
    let before = store.applied_migrations().unwrap();
    drop(store);

    let again = Store::open(&path).unwrap();
    assert_eq!(again.applied_migrations().unwrap(), before);
}

#[test]
fn extension_steps_run_once_in_order_and_are_recorded() {
    let path = temp_db("widgets");
    let store = Store::open_with(&path, vec![Arc::new(Widgets)]).unwrap();
    assert_eq!(
        names(&store, "widgets"),
        ["0001_widgets", "0002_widget_color"]
    );
    store
        .with_conn(|conn| {
            conn.execute("INSERT INTO widgets (name, color) VALUES ('a', 'red')", [])?;
            Ok(())
        })
        .unwrap();
    let before = store.applied_migrations().unwrap();
    drop(store);

    // Reopening with the extension changes nothing; opening without it
    // leaves its tables alone.
    let again = Store::open_with(&path, vec![Arc::new(Widgets)]).unwrap();
    assert_eq!(again.applied_migrations().unwrap(), before);
    drop(again);
    let core_only = Store::open(&path).unwrap();
    let n: i64 = core_only
        .with_conn(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM widgets", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(n, 1);
}

/// A database that predates the registry, at the last single-binary
/// schema: its core steps are adopted, not run, an extension's baseline
/// is adopted the same way, and the core steps added since then run.
#[test]
fn a_schema_14_database_without_a_registry_is_adopted() {
    let path = temp_db("adopt");
    // The core's own first 14 steps stand in for the monolith here (the
    // exact legacy history is exercised where that history lives):
    // build a fresh database, then wind it back to what a schema-14
    // monolith left behind.
    {
        let first = Store::open(&path).unwrap();
        drop(first);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE schema_migrations;
             ALTER TABLE review_log DROP COLUMN client;
             PRAGMA user_version = 14;",
        )
        .unwrap();
    }
    struct Baseline;
    const BASELINE: &[Migration] = &[Migration {
        name: "0001_baseline",
        sql: "CREATE TABLE never_created (id INTEGER PRIMARY KEY) STRICT;",
    }];
    impl StoreExtension for Baseline {
        fn name(&self) -> &'static str {
            "ext"
        }
        fn migrations(&self) -> &'static [Migration] {
            BASELINE
        }
        fn legacy_baseline(&self) -> usize {
            1
        }
    }
    let store = Store::open_with(&path, vec![Arc::new(Baseline)]).unwrap();
    let core = names(&store, "core");
    assert!(core.iter().any(|n| n == MONOLITH_MARKER));
    // The marker, the 14 adopted steps, and v015 applied — once.
    assert_eq!(core.len(), 16);
    assert_eq!(
        core.iter().filter(|n| *n == "v015_review_client").count(),
        1
    );
    assert_eq!(store.schema_version().unwrap(), 15);
    assert_eq!(names(&store, "ext"), ["0001_baseline"]);
    // Adopted, not executed.
    let exists: bool = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'never_created')",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!exists);
}

#[test]
fn an_older_pre_registry_database_is_refused() {
    let path = temp_db("refuse");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY); PRAGMA user_version = 12;")
        .unwrap();
    drop(conn);
    let err = Store::open(&path).err().expect("refused");
    assert!(err.to_string().contains("schema version 12"), "{err}");
}
