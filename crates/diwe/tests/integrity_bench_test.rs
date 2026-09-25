//! Measures `[integrity]` on a real store and prints its debt. Ignored by
//! default; run with
//! `IWE_INTEGRITY_BENCH_STORE=<store dir> cargo test --release -p diwe --test integrity_bench_test -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use diwe::config::{IntegrityMode, IntegrityOptions, ValidationScope};
use diwe::validating_transaction::ValidatingTransaction;
use diwe::fs::new_for_path;
use diwe::integrity::{check_commit, debt, full_debt};
use liwe::graph::Graph;
use liwe::model::config::Format;

fn prefix(key: &str) -> String {
    match key.split_once('/') {
        Some((head, _)) => format!("{head}/"),
        None => "(top level)".to_string(),
    }
}

#[test]
#[ignore]
fn integrity_on_a_real_store() {
    let Ok(store) = std::env::var("IWE_INTEGRITY_BENCH_STORE") else {
        eprintln!("IWE_INTEGRITY_BENCH_STORE not set; skipping");
        return;
    };
    let path = PathBuf::from(store);
    let config = diwe::config::load_config_in(&path).expect("store config");
    let options = config.format_options();

    let started = Instant::now();
    let state = new_for_path(&path, Format::Markdown);
    let read = started.elapsed();

    let started = Instant::now();
    let graph = Graph::from_state(&state, false, options.clone(), config.library.frontmatter_document_title.clone());
    let build = started.elapsed();

    let started = Instant::now();
    let debt_all = full_debt(&graph, "index");
    let debt_time = started.elapsed();

    let started = Instant::now();
    let _ = diwe::integrity::state_digest(&state, "bench");
    let digest_time = started.elapsed();

    let no_new = IntegrityOptions {
        links: IntegrityMode::NoNew,
        orphans: IntegrityMode::NoNew,
        root: "index".to_string(),
    };
    // One commit's gate, as `ValidatingTransaction` runs it: read the store,
    // build the post-commit graph, and (debt present, so no-new compares)
    // the pre-commit graph and its debt.
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let before_state = new_for_path(&path, Format::Markdown);
        let after = Graph::from_state(&before_state.clone(), false, options.clone(), config.library.frontmatter_document_title.clone());
        let result = check_commit(&no_new, &after, || {
            debt(&Graph::from_state(&before_state, false, options.clone(), config.library.frontmatter_document_title.clone()), &no_new)
        });
        assert!(result.is_ok());
        samples.push(started.elapsed());
    }
    samples.sort();

    let mut broken_by: BTreeMap<String, usize> = BTreeMap::new();
    for link in &debt_all.broken_links {
        *broken_by.entry(prefix(link.source_key.as_str())).or_default() += 1;
    }
    let mut orphans_by: BTreeMap<String, usize> = BTreeMap::new();
    for key in &debt_all.orphans {
        *orphans_by.entry(prefix(key.as_str())).or_default() += 1;
    }

    println!("documents: {}", graph.keys().len());
    println!("read store: {read:?}, build graph: {build:?}, debt (broken + reachability): {debt_time:?}, state digest: {digest_time:?}");
    println!(
        "no-new commit gate (read + 2 graph builds + 2 debts): min {:?}, median {:?}, max {:?}",
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() - 1]
    );
    println!("broken links: {}", debt_all.broken_links.len());
    for (prefix, count) in &broken_by {
        println!("  {prefix}: {count}");
    }
    println!("orphans (unreachable from index): {}", debt_all.orphans.len());
    for (prefix, count) in &orphans_by {
        println!("  {prefix}: {count}");
    }

    // End to end: `ValidatingTransaction` commits on a writable copy
    // (`IWE_INTEGRITY_BENCH_WRITABLE`), external checkers left out.
    let Ok(writable) = std::env::var("IWE_INTEGRITY_BENCH_WRITABLE") else {
        return;
    };
    let root = PathBuf::from(writable);
    let mut base = diwe::config::load_config_in(&root).expect("store config");
    base.checkers.clear();
    base.journal = Default::default();
    base.commit = Default::default();
    let page = liwe::model::Key::name("index");
    let original = std::fs::read_to_string(root.join("index.md")).unwrap();
    let variants: Vec<(&str, ValidationScope, IntegrityOptions)> = vec![
        ("validate=none, integrity off", ValidationScope::None, IntegrityOptions::default()),
        ("validate=none, integrity no-new", ValidationScope::None, no_new.clone()),
        ("validate=affected-set-with-checkers, integrity off", ValidationScope::AffectedSetWithCheckers, IntegrityOptions::default()),
        ("validate=affected-set-with-checkers, integrity no-new", ValidationScope::AffectedSetWithCheckers, no_new.clone()),
    ];
    for (label, scope, integrity) in variants {
        let mut config = base.clone();
        config.transactions.validate = scope;
        config.transactions.deny.clear();
        config.integrity = integrity;
        let mut times = Vec::new();
        for i in 0..6 {
            let tx = ValidatingTransaction::new(&root, Format::Markdown, config.clone(), root.join(".iwe/schemas"))
                .with_scope(scope)
                .with_checker_root(&root);
            let content = format!("{original}\n<!-- bench {i} -->\n");
            let started = Instant::now();
            tx.put_one(&page, &content, |_| Ok(())).expect("commit");
            times.push(started.elapsed());
        }
        std::fs::write(root.join("index.md"), &original).unwrap();
        println!(
            "commit [{label}]: first {:?}, then {:?}",
            times[0],
            &times[1..]
        );
    }
}
