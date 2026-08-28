use indoc::indoc;
use liwe::graph::Graph;
use liwe::model::config::MarkdownOptions;
use liwe::query::{argue, ArgueStatus as Status, Argument};
use liwe::state::from_indoc;

fn run(docs: &str) -> Argument {
    let graph = Graph::import(&from_indoc(docs), MarkdownOptions::default(), None);
    argue(&graph)
}

fn statuses(argument: &Argument) -> Vec<(String, Status)> {
    argument
        .nodes
        .iter()
        .map(|n| (n.key.clone(), n.status))
        .collect()
}

fn assert_statuses(argument: &Argument, expected: &[(&str, Status)]) {
    let actual = statuses(argument);
    let expected: Vec<(String, Status)> =
        expected.iter().map(|(k, s)| (k.to_string(), *s)).collect();
    assert_eq!(
        actual,
        expected,
        "\n{}",
        liwe::query::render_argument_text(argument)
    );
}

// 1: claim A.  2: objection B against A.  3: objection C against B.
const REINSTATEMENT: &str = indoc! {"
    ---
    type: fact
    ---
    # A
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # B

    ## Against

    - [A](1)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # C

    ## Against

    - [B](2)
"};

#[test]
fn reinstatement_a_in_b_out_c_in() {
    let argument = run(REINSTATEMENT);
    assert_statuses(
        &argument,
        &[("1", Status::In), ("2", Status::Out), ("3", Status::In)],
    );
    assert_eq!(argument.node("1").unwrap().because, "attackers out (2)");
    assert_eq!(
        argument.node("2").unwrap().because,
        "defeated by '3' (rebuts)"
    );
    assert_eq!(argument.node("3").unwrap().because, "unattacked");
}

// 1: P.  2: Q.  3: X against P, rests on Q.  4: Y against Q, rests on P.
const NIXON_DIAMOND: &str = indoc! {"
    ---
    type: conjecture
    ---
    # P
    _
    ---
    type: conjecture
    ---
    # Q
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # X

    ## Against

    - [P](1)

    ## Rests on

    - [Q](2)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # Y

    ## Against

    - [Q](2)

    ## Rests on

    - [P](1)
"};

#[test]
fn nixon_diamond_is_all_undecided() {
    let argument = run(NIXON_DIAMOND);
    assert_statuses(
        &argument,
        &[
            ("1", Status::Undecided),
            ("2", Status::Undecided),
            ("3", Status::Undecided),
            ("4", Status::Undecided),
        ],
    );
    assert_eq!(
        argument.node("1").unwrap().because,
        "attacker '3' (rebuts) is undecided"
    );
}

// Objections A→B→C→A (each attacks the next).
const ODD_CYCLE: &str = indoc! {"
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # A

    ## Against

    - [B](2)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # B

    ## Against

    - [C](3)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # C

    ## Against

    - [A](1)
"};

#[test]
fn odd_cycle_is_all_undecided() {
    let argument = run(ODD_CYCLE);
    assert_statuses(
        &argument,
        &[
            ("1", Status::Undecided),
            ("2", Status::Undecided),
            ("3", Status::Undecided),
        ],
    );
}

// 1: fact F.  2–6: claims resting on F.  7: objection U against claim 2,
// undermining F.  (8: reply R against U, appended by the second test.)
const SHARED_PREMISE: &str = indoc! {"
    ---
    type: fact
    ---
    # F
    _
    ---
    type: pattern
    ---
    # One

    ## Rests on

    - [F](1)
    _
    ---
    type: pattern
    ---
    # Two

    ## Rests on

    - [F](1)
    _
    ---
    type: model
    ---
    # Three

    ## Rests on

    - [F](1)
    _
    ---
    type: stance
    ---
    # Four

    ## Rests on

    - [F](1)
    _
    ---
    type: conjecture
    ---
    # Five

    ## Rests on

    - [F](1)
    _
    ---
    type: objection
    kind: undermines
    state: open
    ---
    # U

    ## Against

    - [One](2)

    ## Undermines

    - [F](1)
"};

const REPLY: &str = indoc! {"
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # R

    ## Against

    - [U](7)
"};

#[test]
fn undermined_premise_takes_every_dependent_down() {
    let argument = run(SHARED_PREMISE);
    assert_statuses(
        &argument,
        &[
            ("1", Status::Out),
            ("2", Status::Out),
            ("3", Status::Out),
            ("4", Status::Out),
            ("5", Status::Out),
            ("6", Status::Out),
            ("7", Status::In),
        ],
    );
    assert_eq!(
        argument.node("1").unwrap().because,
        "defeated by '7' (undermines)"
    );
    assert_eq!(argument.node("4").unwrap().because, "premise '1' is out");
}

#[test]
fn a_reply_to_the_underminer_reinstates_premise_and_dependents() {
    let docs = format!("{SHARED_PREMISE}\n_\n{REPLY}");
    let argument = run(&docs);
    assert_statuses(
        &argument,
        &[
            ("1", Status::In),
            ("2", Status::In),
            ("3", Status::In),
            ("4", Status::In),
            ("5", Status::In),
            ("6", Status::In),
            ("7", Status::Out),
            ("8", Status::In),
        ],
    );
}

// 1: F.  2: A rests on F.  3: K undercuts A.
const UNDERCUT: &str = indoc! {"
    ---
    type: fact
    ---
    # F
    _
    ---
    type: pattern
    ---
    # A

    ## Rests on

    - [F](1)
    _
    ---
    type: objection
    kind: undercuts
    state: open
    ---
    # K

    ## Against

    - [A](2)
"};

#[test]
fn undercut_defeats_the_claim_and_leaves_its_grounds_standing() {
    let argument = run(UNDERCUT);
    assert_statuses(
        &argument,
        &[("1", Status::In), ("2", Status::Out), ("3", Status::In)],
    );
}

const ANSWERED: &str = indoc! {"
    ---
    type: fact
    ---
    # A
    _
    ---
    type: objection
    kind: rebuts
    state: answered
    ---
    # B

    ## Against

    - [A](1)

    ## Answer

    Revised.
"};

#[test]
fn an_answered_objection_attacks_nothing() {
    let argument = run(ANSWERED);
    assert_statuses(&argument, &[("1", Status::In), ("2", Status::In)]);
    assert_eq!(argument.node("1").unwrap().because, "unattacked");
    assert!(argument.warnings.is_empty());
}

const CONCEDED: &str = indoc! {"
    ---
    type: fact
    ---
    # A
    _
    ---
    type: objection
    kind: rebuts
    state: conceded
    ---
    # B

    ## Against

    - [A](1)

    ## Answer

    Conceded.
"};

#[test]
fn a_conceded_objection_stands_and_its_target_is_reported() {
    let argument = run(CONCEDED);
    assert_statuses(&argument, &[("1", Status::Out), ("2", Status::In)]);
    assert_eq!(argument.warnings.len(), 1);
    assert_eq!(argument.warnings[0].key, "2");
    assert_eq!(
        argument.warnings[0].message,
        "conceded but not demoted: '1' still stands in the graph"
    );
}

// 1: dispute D, resolved by S; thesis and antithesis already deleted.
// 2: claim S resting on D.
const SYNTHESIS: &str = indoc! {"
    ---
    type: dispute
    state: resolved
    ---
    # D

    ## Thesis

    ## Antithesis

    ## Resolution

    - [S](2)
    _
    ---
    type: pattern
    ---
    # S

    ## Rests on

    - [D](1)
"};

#[test]
fn a_synthesis_stands_on_its_resolved_dispute() {
    let argument = run(SYNTHESIS);
    assert_statuses(&argument, &[("2", Status::In)]);
    assert_eq!(argument.disputes.len(), 1);
    let dispute = &argument.disputes[0];
    assert_eq!(dispute.key, "1");
    assert_eq!(dispute.state, "resolved");
    assert!(dispute.thesis.is_none());
    assert!(dispute.antithesis.is_none());
    assert_eq!(dispute.resolution.as_deref(), Some("2"));
    assert_eq!(dispute.decided_by, "resolved by '2'");
    assert!(argument.warnings.is_empty());
}

const DANGLING: &str = indoc! {"
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # Orphan

    ## Against

    - [Gone](9)
"};

#[test]
fn an_objection_whose_target_is_gone_is_a_warning() {
    let argument = run(DANGLING);
    assert_statuses(&argument, &[("1", Status::In)]);
    assert_eq!(argument.warnings.len(), 1);
    assert_eq!(
        argument.warnings[0].message,
        "objection attacks '9', which no longer exists"
    );
}

const OPEN_DISPUTE: &str = indoc! {"
    ---
    type: conjecture
    ---
    # T
    _
    ---
    type: conjecture
    ---
    # A
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # O

    ## Against

    - [T](1)

    ## Rests on

    - [A](2)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # P

    ## Against

    - [A](2)

    ## Rests on

    - [T](1)
    _
    ---
    type: dispute
    state: open
    ---
    # D

    ## Thesis

    - [T](1)

    ## Antithesis

    - [A](2)

    ## Rests on

    - [O](3)
    - [P](4)
"};

#[test]
fn an_open_dispute_names_what_each_side_hangs_on() {
    let argument = run(OPEN_DISPUTE);
    let dispute = &argument.disputes[0];
    assert_eq!(dispute.state, "open");
    assert_eq!(
        dispute.thesis.as_ref().unwrap().status,
        Some(Status::Undecided)
    );
    assert_eq!(
        dispute.decided_by,
        "open — decided by: 1: attacker '3' (rebuts) is undecided; 2: attacker '4' (rebuts) is undecided"
    );
}
