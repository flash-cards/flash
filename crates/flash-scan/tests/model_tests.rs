//! The model reads what the scans rely on, and refuses what it cannot
//! read. Every fixture is a snippet parsed with `parse_source`, so none
//! of them is picked up by the scans that read the crates' `src` trees.

use flash_scan::{compact, parse_source, FnKind, TypeKind, Vis};

#[test]
fn cfg_test_items_are_dropped_wherever_they_sit_and_feature_gates_ship() {
    let file = parse_source(
        "cfg.rs",
        r#"
        #[cfg(test)]
        use std::sync::Mutex;
        fn shipped() { plain(); }
        #[cfg(test)]
        fn hidden() { secret(); }
        #[cfg(not(test))]
        fn kept() {}
        #[cfg(any(test, feature = "test-support"))]
        pub mod harness { pub fn build() {} }
        #[cfg(all(test, feature = "x"))]
        mod gone { pub fn nope() {} }
        #[cfg(feature = "fuzzing")]
        pub mod fuzz { pub fn entry() {} }
        #[cfg(test)]
        mod tests { fn t() { forbidden(); } }
        "#,
    );
    let names: Vec<&str> = file.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["shipped", "kept", "build", "entry"]);
    assert!(file.uses.is_empty());
    assert!(!file
        .calls
        .iter()
        .any(|c| c.callee == "secret" || c.callee == "forbidden"));
    assert!(file.calls.iter().any(|c| c.callee == "plain"));
}

#[test]
fn functions_carry_kind_visibility_params_and_lets() {
    let file = parse_source(
        "fns.rs",
        r#"
        pub struct Services;
        impl Services {
            pub fn deck(&self, user: UserId, deck: DeckId) -> Result<Deck> {
                let id = conn.last_insert_rowid();
                UserId::from_db(id)
            }
            pub(crate) fn helper(user: UserId) {}
            fn private(&self) {}
        }
        pub trait CommunityOps {
            fn publish(&self, user: UserId, deck: DeckId, now: i64) -> Result<()>;
            fn defaulted(&self) {}
        }
        impl<'de> serde::Deserialize<'de> for Thing {
            fn deserialize<D>(d: D) -> Result<Self, D::Error> { Self(x) }
        }
        mod inner { pub fn nested() { fn deeper() {} } }
        "#,
    );
    let deck = file.function("deck").unwrap();
    assert_eq!(
        deck.kind,
        FnKind::Method {
            owner: "Services".into(),
            trait_: None
        }
    );
    assert_eq!(deck.vis, Vis::Pub);
    assert!(deck.has_self);
    assert_eq!(deck.params.len(), 2);
    assert_eq!(deck.params[1].pat, "deck");
    assert!(deck.params[1].ty.idents.contains(&"DeckId".to_string()));
    assert!(deck
        .ret
        .as_ref()
        .unwrap()
        .idents
        .contains(&"Deck".to_string()));
    assert_eq!(deck.lets.len(), 1);
    assert!(deck.lets[0]
        .init
        .as_deref()
        .unwrap()
        .contains("last_insert_rowid("));
    assert_eq!(
        file.function("helper").unwrap().vis,
        Vis::Restricted("crate".into())
    );
    assert_eq!(file.function("private").unwrap().vis, Vis::Private);
    let publish = file.function("publish").unwrap();
    assert_eq!(
        publish.kind,
        FnKind::TraitDecl {
            trait_: "CommunityOps".into(),
            has_default: false
        }
    );
    assert_eq!(publish.vis, Vis::Inherited);
    let deserialize = file.function("deserialize").unwrap();
    assert_eq!(
        deserialize.kind,
        FnKind::Method {
            owner: "Thing".into(),
            trait_: Some("Deserialize".into())
        }
    );
    let nested = file.function("nested").unwrap();
    assert_eq!(nested.module, ["inner"]);
    assert!(file.function("deeper").is_some());
    let from_db = file
        .calls
        .iter()
        .find(|c| c.ends_with(&["UserId", "from_db"]))
        .unwrap();
    assert_eq!(from_db.function.as_deref(), Some("deck"));
    assert_eq!(from_db.args, ["id"]);
    let self_call = file.calls.iter().find(|c| c.path == ["Self"]).unwrap();
    assert_eq!(self_call.function.as_deref(), Some("deserialize"));
}

#[test]
fn types_carry_derives_and_typed_fields_however_written() {
    let file = parse_source(
        "types.rs",
        r#"
        #[derive(
            Debug,
            serde::Deserialize,
        )]
        pub(crate) struct Wrapped {
            #[serde(default, deserialize_with = "crate::bounds::deck_name")]
            pub name: String,
            #[serde(rename = "x", skip_serializing_if = "Option::is_none")]
            pub extra: Option<Vec<String>>,
            pub count: u32,
        }
        #[derive(Deserialize)]
        struct Tuple(String, i64);
        #[derive(Deserialize)]
        enum Choice { Named { note: serde_json::Value }, Plain(Cow<'static, str>), Unit }
        "#,
    );
    let wrapped = file.type_named("Wrapped").unwrap();
    assert_eq!(wrapped.derives, ["Debug", "Deserialize"]);
    assert_eq!(wrapped.vis, Vis::Restricted("crate".into()));
    assert_eq!(
        wrapped.fields[0].deserialize_with.as_deref(),
        Some("crate::bounds::deck_name")
    );
    assert!(wrapped.fields[1].deserialize_with.is_none());
    assert!(wrapped.fields[1].ty.idents.contains(&"Vec".to_string()));
    assert!(!wrapped.fields[2].ty.idents.contains(&"String".to_string()));
    let tuple = file.type_named("Tuple").unwrap();
    assert_eq!(tuple.kind, TypeKind::Struct { tuple: true });
    assert_eq!(tuple.fields.len(), 2);
    assert!(tuple.fields[0].name.is_none());
    let choice = file.type_named("Choice").unwrap();
    assert_eq!(choice.kind, TypeKind::Enum);
    assert_eq!(choice.fields.len(), 2);
    assert_eq!(choice.fields[0].variant.as_deref(), Some("Named"));
    assert!(choice.fields[0].ty.idents.contains(&"Value".to_string()));
    assert!(choice.fields[1].ty.idents.contains(&"Cow".to_string()));
}

#[test]
fn uses_are_flattened_with_renames_and_globs() {
    let file = parse_source(
        "uses.rs",
        r#"
        use std::sync::{Arc, Mutex as M};
        use flash_core::UserId as U;
        use tracing::*;
        "#,
    );
    let paths: Vec<String> = file
        .uses
        .iter()
        .map(|u| {
            format!(
                "{}{}",
                u.path.join("::"),
                u.rename
                    .as_ref()
                    .map(|r| format!(" as {r}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        paths,
        [
            "std::sync::Arc",
            "std::sync::Mutex as M",
            "flash_core::UserId as U",
            "tracing"
        ]
    );
    assert!(file.uses[3].glob);
}

#[test]
fn macros_are_seen_bare_or_qualified_with_their_literals_and_paths() {
    let file = parse_source(
        "macros.rs",
        r#"
        fn f(e: E) {
            tracing::warn!(user = 1, file = %name, "upload failed: {e}");
            warn!("bare {slug}");
            let q = format!("SELECT {COLS} FROM cards WHERE user_id = ?1");
            conn.execute(&format!("DELETE FROM decks"), params![user.raw(), std::sync::Mutex::new(0)]);
        }
        "#,
    );
    let names: Vec<&str> = file.macros.iter().map(|m| m.last()).collect();
    assert_eq!(names, ["warn", "warn", "format", "format", "params"]);
    assert_eq!(file.macros[0].path, ["tracing", "warn"]);
    assert_eq!(file.macros[1].path, ["warn"]);
    let sql: Vec<&str> = file
        .string_literals
        .iter()
        .filter(|l| l.in_macro && l.value.contains("FROM"))
        .map(|l| l.value.as_str())
        .collect();
    assert_eq!(
        sql,
        [
            "SELECT {COLS} FROM cards WHERE user_id = ?1",
            "DELETE FROM decks"
        ]
    );
    assert!(file
        .string_literals
        .iter()
        .all(|l| l.function.as_deref() == Some("f")));
    assert!(file
        .paths
        .iter()
        .any(|p| p.in_macro && p.contains_run(&["std", "sync", "Mutex"])));
    assert_eq!(file.macros[0].line, 3);
}

#[test]
fn calls_keep_qualified_self_types_statements_and_receivers() {
    let file = parse_source(
        "calls.rs",
        r#"
        fn f() {
            let x = rows.map(|(id, role)| (<UserId>::from_db(id), role));
            self.calls.lock().unwrap().push(1);
            Failure::user(format!("no deck named '{name}': {e}"));
        }
        "#,
    );
    let from_db = file
        .calls
        .iter()
        .find(|c| c.ends_with(&["UserId", "from_db"]))
        .unwrap();
    assert!(from_db.qself.is_some());
    assert!(from_db.stmt.contains("rows.map("));
    let unwrap = file
        .method_calls
        .iter()
        .find(|m| m.method == "unwrap")
        .unwrap();
    assert_eq!(unwrap.receiver_method.as_deref(), Some("lock"));
    let user = file
        .calls
        .iter()
        .find(|c| c.ends_with(&["Failure", "user"]))
        .unwrap();
    assert_eq!(user.args, [r#"format!("no deck named '{name}': {e}")"#]);
}

#[test]
fn attributes_are_kept_compact_and_doc_comments_are_not_code() {
    let file = parse_source(
        "attrs.rs",
        r#"
        /// SELECT * FROM cards
        #[tool(
            description = "Deletes a deck",
            annotations(destructive_hint = true, idempotent_hint = false)
        )]
        async fn delete_deck(&self, Parameters(args): Parameters<DeleteDeckArgs>) {}
        "#,
    );
    let f = file.function("delete_deck").unwrap();
    assert!(f.is_async);
    let tool = f.attr("tool").unwrap();
    assert!(tool.args.contains("destructive_hint=true"));
    assert!(f.attrs.iter().all(|a| a.path != ["doc"]));
    assert!(file
        .string_literals
        .iter()
        .all(|l| !l.value.contains("FROM")));
    assert!(f.params[0].pat_idents.contains(&"Parameters".to_string()));
    assert!(f.params[0]
        .ty
        .idents
        .contains(&"DeleteDeckArgs".to_string()));
}

#[test]
fn compact_drops_every_separator_and_keeps_literal_text() {
    let tokens: proc_macro2::TokenStream = syn::parse_str("a . b ( \"x y\" , 1 )").unwrap();
    assert_eq!(compact(&tokens), "a.b(\"x y\",1)");
}

#[test]
fn a_file_that_does_not_parse_is_a_panic_not_a_gap() {
    let result = std::panic::catch_unwind(|| parse_source("broken.rs", "fn f( {"));
    assert!(result.is_err());
}

#[test]
fn routes_read_chains_qualified_constructors_filters_and_consts() {
    let dir = std::env::temp_dir().join(format!("flash-scan-routes-{}", std::process::id()));
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("api")).unwrap();
    std::fs::write(
        src.join("web.rs"),
        r#"
        pub const HOOK: &str = "/hook";
        pub fn router() -> Router {
            Router::new()
                .route("/decks", get(list).post(create))
                .route("/x", axum::routing::put(set).layer(DefaultBodyLimit::max(1)))
                .route("/y", on(MethodFilter::PATCH, patch_it))
                .route("/z", get_service(svc))
                .route(HOOK, post(hook))
        }
        "#,
    )
    .unwrap();
    std::fs::write(
        src.join("api").join("decks.rs"),
        r#"pub fn router() -> Router { Router::new().route("/decks", get(list)) }"#,
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"pub fn app() -> Router { Router::new().nest("/api/v1", v1).nest_service("/mcp", mcp) }"#,
    )
    .unwrap();
    let routes = flash_scan::routes::inventory(&[&dir]);
    let lines: Vec<String> = routes
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(
        lines,
        [
            "DELETE /mcp",
            "GET /api/v1/decks",
            "GET /decks",
            "GET /mcp",
            "GET /z",
            "PATCH /y",
            "POST /decks",
            "POST /hook",
            "POST /mcp",
            "PUT /x",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_route_the_scanner_cannot_read_is_a_panic() {
    for (label, body) in [
        (
            "macro",
            r#"macro_rules! r { () => { Router::new().route("/m", get(h)) } }"#,
        ),
        (
            "path",
            r#"fn f() -> Router { Router::new().route(dynamic(), get(h)) }"#,
        ),
        (
            "methods",
            r#"fn f() -> Router { Router::new().route("/n", handler) }"#,
        ),
        (
            "nest",
            r#"fn f() -> Router { Router::new().nest("/other", r) }"#,
        ),
    ] {
        let dir =
            std::env::temp_dir().join(format!("flash-scan-refuse-{label}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("web.rs"), body).unwrap();
        let result = std::panic::catch_unwind(|| flash_scan::routes::inventory(&[&dir]));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err(), "{label} was not refused");
    }
}
