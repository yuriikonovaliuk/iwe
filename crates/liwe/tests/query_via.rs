use crate::queries::{any_document, filter, referenced_by, references};
use indoc::indoc;
use liwe::graph::Graph;
use liwe::model::config::MarkdownOptions;
use liwe::query::block::BlockPredicate;
use liwe::query::execute;
use liwe::query::{
    parse_operation, CountPred, Filter, FindOp, OperationKind, Outcome, ReferenceAnchor,
};
use liwe::state::from_indoc;

fn keys(docs: &str, op: FindOp) -> Vec<String> {
    let graph = Graph::import(&from_indoc(docs), MarkdownOptions::default(), None);
    match execute(&liwe::query::Operation::Find(op), &graph).expect("query succeeds") {
        Outcome::Find { matches, .. } => matches.into_iter().map(|m| m.key.to_string()).collect(),
        other => panic!("expected Find, got {:?}", other),
    }
}

fn via_section(section: &str) -> BlockPredicate {
    BlockPredicate::empty().within_section(section)
}

// 1: Is a → 2, prose → 3.  2: Is a → 4.  3: prose → 4.  4: nothing.
const CHAIN: &str = indoc! {"
    # Alpha

    Alpha mentions [gamma](3) in passing.

    ## Is a

    - [Beta](2)
    _
    # Beta

    ## Is a

    - [Delta](4)
    _
    # Gamma

    Gamma links [delta](4) in prose.
    _
    # Delta
"};

#[test]
fn via_restricts_direct_edges_to_the_section() {
    assert_eq!(
        keys(
            CHAIN,
            filter(references(
                ReferenceAnchor::with_max("2", 1).with_via(via_section("Is a"))
            )),
        ),
        vec!["1"]
    );
    // The prose link from 1 to 3 is not inside "Is a", so it is not an edge.
    assert_eq!(
        keys(
            CHAIN,
            filter(references(
                ReferenceAnchor::with_max("3", 1).with_via(via_section("Is a"))
            )),
        ),
        Vec::<String>::new()
    );
    // Without via the prose link counts.
    assert_eq!(
        keys(CHAIN, filter(references(ReferenceAnchor::with_max("3", 1)))),
        vec!["1"]
    );
}

#[test]
fn via_applies_at_every_hop_of_a_transitive_walk() {
    // 1 → 2 → 4 along "Is a"; 3 reaches 4 only through prose.
    assert_eq!(
        keys(
            CHAIN,
            filter(references(
                ReferenceAnchor::with_max("4", u32::MAX).with_via(via_section("Is a"))
            )),
        ),
        vec!["1", "2"]
    );
    assert_eq!(
        keys(
            CHAIN,
            filter(references(ReferenceAnchor::with_max("4", u32::MAX))),
        ),
        vec!["1", "2", "3"]
    );
}

#[test]
fn via_on_referenced_by_walks_the_chain_downward() {
    assert_eq!(
        keys(
            CHAIN,
            filter(referenced_by(
                ReferenceAnchor::with_max("1", u32::MAX).with_via(via_section("Is a"))
            )),
        ),
        vec!["2", "4"]
    );
}

#[test]
fn via_counts_only_scoped_links() {
    // Documents with exactly one "Is a" link: 1 and 2.
    assert_eq!(
        keys(
            CHAIN,
            filter(references(
                ReferenceAnchor::with_match(any_document(), 1, 1)
                    .with_size(CountPred::eq(1))
                    .with_via(via_section("Is a"))
            )),
        ),
        vec!["1", "2"]
    );
}

#[test]
fn via_accepts_a_block_predicate_mapping() {
    let op = parse_operation(
        indoc! {"
            filter:
              $references:
                match: { $key: '2' }
                via: { $within: { $section: Is a } }
        "},
        OperationKind::Find,
    )
    .expect("parses");
    let expected = liwe::query::Operation::Find(filter(references(
        ReferenceAnchor::with_max("2", 1).with_via(via_section("Is a")),
    )));
    assert_eq!(op, expected);
}

#[test]
fn via_string_is_shorthand_for_within_section() {
    let op = parse_operation(
        indoc! {"
            filter:
              $references:
                match: { $key: '2' }
                via: Is a
        "},
        OperationKind::Find,
    )
    .expect("parses");
    let expected = liwe::query::Operation::Find(filter(references(
        ReferenceAnchor::with_max("2", 1).with_via(via_section("Is a")),
    )));
    assert_eq!(op, expected);
}

#[test]
fn via_is_rejected_on_inclusion_operators() {
    let err = parse_operation(
        indoc! {"
            filter:
              $includedBy:
                match: { $key: '2' }
                via: Is a
        "},
        OperationKind::Find,
    )
    .expect_err("via on $includedBy must fail");
    assert!(format!("{err}").contains("via"), "{err}");
}

#[test]
fn via_over_a_missing_section_yields_no_edges() {
    let f: Filter = references(ReferenceAnchor::with_max("2", 1).with_via(via_section("Nope")));
    assert_eq!(keys(CHAIN, filter(f)), Vec::<String>::new());
}
