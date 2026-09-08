//! Opt-in end-to-end check against the locally built torture deck
//! (`python tools/deck-testing/torture_deck.py target/deck-tests/torture.apkg`).
//! Ignored by default: the file isn't in the repo.
//! Run: `cargo test -p flash-store --test torture_deck_test -- --ignored`

use flash_store::import::parse_apkg;

/// Opt-in stress run over any deck: `FLASH_STRESS_PATH=<file> cargo test
/// -p flash-store --test torture_deck_test stress -- --ignored --nocapture`.
/// Parses, audits every card for sanitizer escapes, exercises the
/// clean-look strip pass and the pending-import serde round-trip, and
/// prints throughput numbers.
#[test]
#[ignore]
fn stress_any_deck() {
    let path = std::env::var("FLASH_STRESS_PATH").expect("set FLASH_STRESS_PATH");
    let t0 = std::time::Instant::now();
    let bytes = std::fs::read(&path).expect("read deck");
    let read_ms = t0.elapsed().as_millis();

    let t1 = std::time::Instant::now();
    let parsed = parse_apkg(&bytes).expect("parse");
    let parse_ms = t1.elapsed().as_millis();

    let mut rich = 0usize;
    let mut colored = 0usize;
    let mut collapsibles = 0usize;
    let mut cloze = 0usize;
    let mut html_bytes = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for (i, row) in parsed.rows.iter().enumerate() {
        for html in [row.front_html.as_deref(), row.back_html.as_deref()]
            .into_iter()
            .flatten()
        {
            rich += 1;
            html_bytes += html.len();
            if html.contains("hl-") {
                colored += 1;
            }
            if html.contains("<details") {
                collapsibles += 1;
            }
            if html.contains("cloze-blank") || html.contains("cloze-answer") {
                cloze += 1;
            }
            for bad in [
                "<script",
                "onclick",
                "onerror",
                "onload",
                "javascript:",
                "style=",
                "<iframe",
            ] {
                if html.contains(bad) {
                    violations.push(format!("row {i}: contains {bad}"));
                }
            }
        }
        for text in [&row.front, &row.back] {
            if text.contains("<script") || text.contains("javascript:") {
                violations.push(format!("row {i}: plain text leak"));
            }
        }
    }
    assert!(violations.is_empty(), "sanitizer escapes: {violations:?}");

    // Clean-look pass over every card (the commit-time strip path).
    let t2 = std::time::Instant::now();
    let mut stripped_hl = 0usize;
    for row in &parsed.rows {
        for html in [row.front_html.as_deref(), row.back_html.as_deref()]
            .into_iter()
            .flatten()
        {
            let s = flash_store::richtext::strip_highlight_classes(html);
            assert!(!s.contains("hl-"), "strip left highlights behind");
            if s.len() != html.len() {
                stripped_hl += 1;
            }
        }
    }
    let strip_ms = t2.elapsed().as_millis();

    // Preview persistence round-trip (what PendingImport does on disk).
    let t3 = std::time::Instant::now();
    let json = serde_json::to_vec(&parsed.rows).expect("serialize rows");
    let back: Vec<flash_store::import::ImportRow> =
        serde_json::from_slice(&json).expect("deserialize rows");
    assert_eq!(back.len(), parsed.rows.len());
    let serde_ms = t3.elapsed().as_millis();

    println!("== stress: {path}");
    println!(
        "   {} bytes read in {read_ms}ms; parsed {} rows ({} skipped) in {parse_ms}ms",
        bytes.len(),
        parsed.rows.len(),
        parsed.skipped
    );
    println!(
        "   rich sides: {rich} ({html_bytes} bytes) | colored: {colored} | collapsibles: {collapsibles} | cloze: {cloze}"
    );
    println!("   clean-look strip over all cards: {strip_ms}ms ({stripped_hl} sides changed)");
    println!(
        "   pending-import serde round-trip: {serde_ms}ms ({} bytes json)",
        json.len()
    );
    println!("   messages: {:?}", parsed.messages);
}

#[test]
#[ignore]
fn torture_deck_parses_with_fidelity_features() {
    let path = std::env::var("FLASH_TORTURE_PATH")
        .unwrap_or_else(|_| "../../target/deck-tests/torture.apkg".into());
    let bytes = std::fs::read(&path).expect("build the torture deck first");
    let parsed = parse_apkg(&bytes).expect("torture deck parses");
    assert!(
        parsed.rows.len() > 20,
        "expected many cards, got {}",
        parsed.rows.len()
    );

    let all_html: String = parsed
        .rows
        .iter()
        .flat_map(|r| {
            [
                r.front_html.as_deref().unwrap_or(""),
                r.back_html.as_deref().unwrap_or(""),
            ]
        })
        .collect();
    // XSS probes never survive any pipeline stage.
    assert!(!all_html.contains("<script"), "script leaked");
    assert!(
        !all_html.contains("onerror") && !all_html.contains("onclick"),
        "handler leaked"
    );
    assert!(!all_html.contains("javascript:"), "js url leaked");
    // Cloze structure still renders.
    assert!(all_html.contains("cloze-blank"), "cloze blanks present");
    let all_text: String = parsed
        .rows
        .iter()
        .flat_map(|r| [r.front.as_str(), r.back.as_str()])
        .collect();
    assert!(
        !all_text.contains("alert("),
        "script text leaked into plain text"
    );
}
