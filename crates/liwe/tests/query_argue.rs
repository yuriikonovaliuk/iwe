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

#[test]
fn a_circular_ground_is_a_warning() {
    let argument = run(OPEN_DISPUTE);
    let messages: Vec<&str> = argument
        .warnings
        .iter()
        .map(|w| w.message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "circular ground: enters '5' against '1' and rests on the other side '2'",
            "circular ground: enters '5' against '2' and rests on the other side '1'",
        ]
    );
}

// 1: A.  2: objection against A that rests on A.
const SELF_GROUNDED: &str = indoc! {"
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

    ## Rests on

    - [A](1)
"};

#[test]
fn resting_on_the_attacked_claim_is_a_warning() {
    let argument = run(SELF_GROUNDED);
    assert_eq!(
        argument.warnings[0].message,
        "circular ground: rests on '1', the claim it attacks"
    );
}

#[test]
fn diagnosis_reduces_a_diamond_to_one_root_with_its_moves() {
    let argument = run(OPEN_DISPUTE);
    let diagnosis = liwe::query::diagnose(&argument);
    assert_eq!(diagnosis.roots.len(), 1);
    let root = &diagnosis.roots[0];
    assert_eq!(root.id, 1);
    assert_eq!(root.members, vec!["1", "2", "3", "4"]);
    assert_eq!(root.disputes, vec!["5"]);
    assert!(root.hangs_on.is_empty());
    let moves: Vec<(&str, &str)> = root
        .moves
        .iter()
        .map(|m| (m.key.as_str(), m.what.as_str()))
        .collect();
    assert_eq!(
        moves,
        vec![
            ("3", "circular ground: enters '5' against '1' and rests on the other side '2' — give it a ground outside the cycle, or answer it"),
            ("4", "circular ground: enters '5' against '2' and rests on the other side '1' — give it a ground outside the cycle, or answer it"),
        ]
    );
    assert!(diagnosis.downstream.is_empty());
    assert!(diagnosis.defeated.is_empty());
    assert!(diagnosis.pending.is_empty());
    let text = liwe::query::render_diagnosis_text(&diagnosis);
    assert!(
        text.starts_with("roots (1):\n  #1  cycle of 4: 1, 2, 3, 4\n      dispute: 5\n"),
        "{text}"
    );
}

// The diamond of OPEN_DISPUTE plus 6: a pattern resting on T (downstream of
// the root), and 7: a hypothesis with a pending test in its own open dispute 8.
const DOWNSTREAM_AND_PENDING: &str = indoc! {"
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
    _
    ---
    type: pattern
    ---
    # S

    ## Rests on

    - [T](1)
    _
    ---
    type: hypothesis
    test_state: pending
    ---
    # H
    _
    ---
    type: dispute
    state: open
    ---
    # E

    ## Thesis

    - [H](7)

    ## Antithesis

    - [S](6)
"};

#[test]
fn diagnosis_lists_downstream_claims_and_pending_hypotheses() {
    let argument = run(DOWNSTREAM_AND_PENDING);
    let diagnosis = liwe::query::diagnose(&argument);
    assert_eq!(diagnosis.roots.len(), 1);
    assert_eq!(diagnosis.downstream.len(), 1);
    assert_eq!(diagnosis.downstream[0].key, "6");
    assert_eq!(diagnosis.downstream[0].roots, vec![1]);
    assert_eq!(diagnosis.pending.len(), 1);
    assert_eq!(diagnosis.pending[0].key, "7");
    assert_eq!(diagnosis.pending[0].dispute, "8");

    let mut selected = diagnosis.clone();
    selected.select(&["6".to_string()].into_iter().collect());
    assert_eq!(
        selected.roots.len(),
        1,
        "a selected downstream node keeps its root"
    );
    assert_eq!(selected.downstream.len(), 1);
    assert!(selected.pending.is_empty());
}

#[test]
fn diagnosis_names_the_reinstatement_moves_of_a_defeated_claim() {
    let argument = run(UNDERCUT);
    let diagnosis = liwe::query::diagnose(&argument);
    assert_eq!(diagnosis.defeated.len(), 1);
    let d = &diagnosis.defeated[0];
    assert_eq!(d.key, "2");
    assert_eq!(d.because, "defeated by '3' (undercuts)");
    assert_eq!(
        d.moves,
        vec![
            "answer '3' (state: answered) — revise the claim to meet it",
            "concede it and demote or delete the claim",
            "attack '3' with a counter-objection grounded outside this dispute",
        ]
    );
}

#[test]
fn standing_is_a_filter_operator() {
    use liwe::query::{evaluate, parse_filter_expression};
    let graph = Graph::import(&from_indoc(REINSTATEMENT), MarkdownOptions::default(), None);
    let keys = |expr: &str| -> Vec<String> {
        evaluate(&parse_filter_expression(expr).unwrap(), &graph)
            .into_iter()
            .map(|k| k.to_string())
            .collect()
    };
    assert_eq!(keys("$standing: in"), vec!["1", "3"]);
    assert_eq!(keys("$standing: { $ne: in }"), vec!["2"]);
    assert_eq!(keys("$standing: { $in: [out, undecided] }"), vec!["2"]);
    assert_eq!(keys("type: objection, $standing: in"), vec!["3"]);
    assert_eq!(
        keys("type: fact, $standing: { $nin: [in] }"),
        Vec::<String>::new()
    );
    assert!(parse_filter_expression("$standing: maybe")
        .unwrap_err()
        .to_string()
        .contains("'$standing' expects in, out or undecided"));
}

// 1: a generic fact.  2: a particular objection against it.  3: a universal
// claim.  4: a particular objection against it (a counter-instance).
const PARTICULAR_VS_GENERIC: &str = indoc! {"
    ---
    type: fact
    quantity: generic
    ---
    # Defects scale with code
    _
    ---
    type: objection
    kind: undercuts
    state: open
    quantity: particular
    ---
    # Not when the lines are a wrong abstraction

    ## Against

    - [F](1)
    _
    ---
    type: pattern
    quantity: universal
    ---
    # Every objection has a ground
    _
    ---
    type: objection
    kind: rebuts
    state: open
    quantity: particular
    ---
    # This one has none

    ## Against

    - [U](3)
"};

#[test]
fn a_particular_is_an_exception_to_a_generic_but_refutes_a_universal() {
    let argument = run(PARTICULAR_VS_GENERIC);
    assert_statuses(
        &argument,
        &[
            ("1", Status::In),
            ("2", Status::In),
            ("3", Status::Out),
            ("4", Status::In),
        ],
    );
    assert_eq!(argument.node("1").unwrap().because, "unattacked");
    assert_eq!(
        argument.node("1").unwrap().quantity.as_deref(),
        Some("generic")
    );
    assert_eq!(argument.warnings.len(), 1);
    assert_eq!(
        argument.warnings[0].message,
        "exception: a particular objection does not defeat the generic '1' — absorb it into the claim's scope and answer it"
    );
}

// 1: an axiom.  2: a fact resting on it.  3: an objection against the axiom.
const AXIOM: &str = indoc! {"
    ---
    type: axiom
    ---
    # Non-contradiction

    ## Statement

    The same attribute cannot at the same time belong and not belong to the same subject in the same respect.
    _
    ---
    type: fact
    ---
    # F

    ## Rests on

    - [A](1)
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # Contradictions are true

    ## Against

    - [A](1)
"};

#[test]
fn an_axiom_stands_and_cannot_be_attacked() {
    let argument = run(AXIOM);
    assert_statuses(
        &argument,
        &[("1", Status::In), ("2", Status::In), ("3", Status::In)],
    );
    assert_eq!(argument.node("1").unwrap().kind, "axiom");
    assert_eq!(
        argument.warnings[0].message,
        "an axiom cannot be attacked: '1' — its denial presupposes it"
    );
}

// 1: a fact with two sentences.  2: an objection quoting one of them.
// 3: an objection quoting a sentence that is not there.
const DENIES: &str = indoc! {"
    ---
    type: fact
    ---
    # Defects scale with code

    Defect density per [KLOC](9) is roughly *fixed*, so volume is a risk quantity by itself. Owning less code means fewer defects.
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # Volume is not a risk quantity

    ## Against

    - [F](1)

    ## Denies

    > volume is a risk quantity by itself
    _
    ---
    type: objection
    kind: rebuts
    state: open
    ---
    # Fewer lines is not less liability

    ## Against

    - [F](1)

    ## Denies

    > fewer lines is less liability
"};

#[test]
fn a_rebuttal_must_quote_what_it_denies() {
    let argument = run(DENIES);
    let messages: Vec<&str> = argument
        .warnings
        .iter()
        .map(|w| w.message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec!["denies nothing in '1': the quoted sentence is not in it"]
    );
    assert_eq!(argument.warnings[0].key, "3");
}

// 1 rests on 2, 2 rests on 1; 3 rests on 1.
const SUPPORT_CYCLE: &str = indoc! {"
    ---
    type: pattern
    ---
    # A

    ## Rests on

    - [B](2)
    _
    ---
    type: pattern
    ---
    # B

    ## Rests on

    - [A](1)
    _
    ---
    type: pattern
    ---
    # C

    ## Rests on

    - [A](1)
"};

#[test]
fn a_support_cycle_is_a_warning_and_stays_undecided() {
    let argument = run(SUPPORT_CYCLE);
    assert_statuses(
        &argument,
        &[
            ("1", Status::Undecided),
            ("2", Status::Undecided),
            ("3", Status::Undecided),
        ],
    );
    assert_eq!(argument.warnings.len(), 1);
    assert_eq!(
        argument.warnings[0].message,
        "support cycle: 1 → 2 — a chain of Rests on that returns to itself never reaches the floor"
    );
}
