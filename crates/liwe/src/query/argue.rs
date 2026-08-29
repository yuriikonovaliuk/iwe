//! Computed acceptability of the claims in a graph — dialectical reasoning
//! over objections, with grounded semantics and deductive support.
//!
//! Nodes are the claim-bearing documents (`fact`, `pattern`, `model`,
//! `stance`, `conjecture`, `hypothesis`) and the `objection`s. An objection
//! attacks the target of its `Against` section — or, when its `kind` is
//! `undermines`, the premise named in its `Undermines` section. `Rests on`
//! links between nodes are support edges. An objection in `state: answered`
//! attacks nothing (the claim was revised to meet it); one that is `conceded`
//! stands, and the graph is told if its target is still present.
//!
//! Standing is the unique grounded extension, extended with deductive
//! support: a node is *in* when every attacker is out and every premise is
//! in; *out* when some attacker is in or some premise is out; otherwise
//! *undecided*. Nothing is accepted that the argument does not force.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::graph::Graph;
use crate::model::Key;
use crate::query::block::BlockPredicate;
use crate::query::block_eval::BlockIndex;

pub const CLAIM_TYPES: [&str; 6] = [
    "fact",
    "pattern",
    "model",
    "stance",
    "conjecture",
    "hypothesis",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    In,
    Out,
    Undecided,
}

impl Status {
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "in" => Some(Status::In),
            "out" => Some(Status::Out),
            "undecided" => Some(Status::Undecided),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::In => "in",
            Status::Out => "out",
            Status::Undecided => "undecided",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Attacker {
    pub key: String,
    pub kind: String,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize)]
pub struct Premise {
    pub key: String,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_state: Option<String>,
    pub status: Status,
    pub attackers: Vec<Attacker>,
    pub premises: Vec<Premise>,
    pub because: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Side {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dispute {
    pub key: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thesis: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antithesis: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// What decides the dispute: the resolution, or — while it is open — the
    /// attacker or premise on which each side's standing hangs.
    pub decided_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub key: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Argument {
    pub nodes: Vec<Node>,
    pub disputes: Vec<Dispute>,
    pub warnings: Vec<Warning>,
}

impl Argument {
    pub fn status(&self, key: &str) -> Option<Status> {
        self.nodes.iter().find(|n| n.key == key).map(|n| n.status)
    }

    pub fn node(&self, key: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.key == key)
    }
}

/// (key, state, thesis, antithesis, resolution)
type RawDispute = (Key, String, Vec<Key>, Vec<Key>, Vec<Key>);

struct Raw {
    key: Key,
    kind: String,
    state: Option<String>,
    test_state: Option<String>,
    objection_kind: Option<String>,
    against: Vec<Key>,
    undermines: Vec<Key>,
    rests_on: Vec<Key>,
}

fn field(graph: &Graph, key: &Key, name: &str) -> Option<String> {
    graph
        .frontmatter(key)
        .and_then(|fm| fm.get(serde_yaml::Value::String(name.to_string())))
        .and_then(|v| match v {
            serde_yaml::Value::String(s) => Some(s.clone()),
            serde_yaml::Value::Number(n) => Some(n.to_string()),
            serde_yaml::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
}

fn section_targets(index: &BlockIndex, section: &str) -> Vec<Key> {
    index.targets_within(&BlockPredicate::empty().within_section(section))
}

/// Compute the standing of every claim and objection in the graph.
pub fn argue(graph: &Graph) -> Argument {
    let mut keys = graph.keys();
    keys.sort_by_key(|k| k.to_string());

    let mut raws: Vec<Raw> = Vec::new();
    let mut disputes_raw: Vec<RawDispute> = Vec::new();
    for key in &keys {
        let Some(kind) = field(graph, key, "type") else {
            continue;
        };
        if kind == "dispute" {
            let index = BlockIndex::build(graph, key);
            disputes_raw.push((
                key.clone(),
                field(graph, key, "state").unwrap_or_else(|| "open".to_string()),
                section_targets(&index, "Thesis"),
                section_targets(&index, "Antithesis"),
                section_targets(&index, "Resolution"),
            ));
            continue;
        }
        let is_claim = CLAIM_TYPES.contains(&kind.as_str());
        let is_objection = kind == "objection";
        if !is_claim && !is_objection {
            continue;
        }
        let index = BlockIndex::build(graph, key);
        raws.push(Raw {
            key: key.clone(),
            kind: kind.clone(),
            state: if is_objection {
                Some(field(graph, key, "state").unwrap_or_else(|| "open".to_string()))
            } else {
                None
            },
            test_state: if kind == "hypothesis" {
                Some(field(graph, key, "test_state").unwrap_or_else(|| "pending".to_string()))
            } else {
                None
            },
            objection_kind: if is_objection {
                Some(field(graph, key, "kind").unwrap_or_else(|| "rebuts".to_string()))
            } else {
                None
            },
            against: if is_objection {
                section_targets(&index, "Against")
            } else {
                Vec::new()
            },
            undermines: if is_objection {
                section_targets(&index, "Undermines")
            } else {
                Vec::new()
            },
            rests_on: section_targets(&index, "Rests on"),
        });
    }

    let position: HashMap<Key, usize> = raws
        .iter()
        .enumerate()
        .map(|(i, raw)| (raw.key.clone(), i))
        .collect();
    let n = raws.len();

    // attackers[i]: (objection index, kind) — live objections only.
    let mut attackers: Vec<Vec<(usize, String)>> = vec![Vec::new(); n];
    let mut premises: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut warnings = Vec::new();
    for (i, raw) in raws.iter().enumerate() {
        for premise in &raw.rests_on {
            if let Some(&p) = position.get(premise) {
                if p != i && !premises[i].contains(&p) {
                    premises[i].push(p);
                }
            }
        }
        if raw.kind != "objection" {
            continue;
        }
        let objection_kind = raw.objection_kind.clone().unwrap_or_default();
        let target = if objection_kind == "undermines" && !raw.undermines.is_empty() {
            raw.undermines.first()
        } else {
            raw.against.first()
        };
        let Some(target) = target else {
            warnings.push(Warning {
                key: raw.key.to_string(),
                message: "objection attacks nothing: its Against section names no document"
                    .to_string(),
            });
            continue;
        };
        let Some(&t) = position.get(target) else {
            let present = graph.maybe_key(target).is_some();
            warnings.push(Warning {
                key: raw.key.to_string(),
                message: if present {
                    format!("objection attacks '{target}', which is not a claim or objection")
                } else {
                    format!("objection attacks '{target}', which no longer exists")
                },
            });
            continue;
        };
        if raw.state.as_deref() == Some("answered") {
            continue;
        }
        if t != i {
            attackers[t].push((i, objection_kind));
        }
    }

    // Grounded fixpoint with deductive support.
    let mut status = vec![Status::Undecided; n];
    loop {
        let mut changed = false;
        for i in 0..n {
            if status[i] != Status::Undecided {
                continue;
            }
            let attacked = attackers[i].iter().any(|(a, _)| status[*a] == Status::In);
            let premise_out = premises[i].iter().any(|p| status[*p] == Status::Out);
            if attacked || premise_out {
                status[i] = Status::Out;
                changed = true;
                continue;
            }
            let all_attackers_out = attackers[i].iter().all(|(a, _)| status[*a] == Status::Out);
            let all_premises_in = premises[i].iter().all(|p| status[*p] == Status::In);
            if all_attackers_out && all_premises_in {
                status[i] = Status::In;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Conceded objections whose target is still present.
    for (i, raw) in raws.iter().enumerate() {
        if raw.kind == "objection" && raw.state.as_deref() == Some("conceded") {
            for (t, list) in attackers.iter().enumerate() {
                if list.iter().any(|(a, _)| *a == i) {
                    warnings.push(Warning {
                        key: raw.key.to_string(),
                        message: format!(
                            "conceded but not demoted: '{}' still stands in the graph",
                            raws[t].key
                        ),
                    });
                }
            }
        }
    }

    // Circular grounds: an objection whose premises (transitively) include
    // the claim it attacks, or the other side of a dispute it enters.
    for (i, raw) in raws.iter().enumerate() {
        if raw.kind != "objection" {
            continue;
        }
        let Some(target) = raw.against.first() else {
            continue;
        };
        let closure = premise_closure(&premises, i);
        if let Some(&t) = position.get(target) {
            if closure.contains(&t) {
                warnings.push(Warning {
                    key: raw.key.to_string(),
                    message: format!(
                        "circular ground: rests on '{}', the claim it attacks",
                        raws[t].key
                    ),
                });
            }
        }
        for (dkey, _, thesis, antithesis, _) in &disputes_raw {
            let other = if thesis.first() == Some(target) {
                antithesis.first()
            } else if antithesis.first() == Some(target) {
                thesis.first()
            } else {
                None
            };
            let Some(other) = other else {
                continue;
            };
            if let Some(&o) = position.get(other) {
                if closure.contains(&o) {
                    warnings.push(Warning {
                        key: raw.key.to_string(),
                        message: format!(
                            "circular ground: enters '{dkey}' against '{target}' and rests on the other side '{other}'"
                        ),
                    });
                }
            }
        }
    }

    let nodes: Vec<Node> = raws
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let attackers: Vec<Attacker> = attackers[i]
                .iter()
                .map(|(a, kind)| Attacker {
                    key: raws[*a].key.to_string(),
                    kind: kind.clone(),
                    status: status[*a],
                })
                .collect();
            let premises: Vec<Premise> = premises[i]
                .iter()
                .map(|p| Premise {
                    key: raws[*p].key.to_string(),
                    status: status[*p],
                })
                .collect();
            let because = explain(status[i], &attackers, &premises);
            Node {
                key: raw.key.to_string(),
                kind: raw.kind.clone(),
                state: raw.state.clone(),
                test_state: raw.test_state.clone(),
                status: status[i],
                attackers,
                premises,
                because,
            }
        })
        .collect();

    let by_key: HashMap<&str, &Node> = nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    let side = |targets: &[Key]| -> Option<Side> {
        targets.first().map(|k| Side {
            key: k.to_string(),
            status: by_key.get(k.to_string().as_str()).map(|n| n.status),
        })
    };
    let disputes: Vec<Dispute> = disputes_raw
        .iter()
        .map(|(key, state, thesis, antithesis, resolution)| {
            let thesis = side(thesis);
            let antithesis = side(antithesis);
            let resolution = resolution.first().map(|k| k.to_string());
            let decided_by = if state == "resolved" {
                match &resolution {
                    Some(r) => format!("resolved by '{r}'"),
                    None => "resolved, but names no resolution".to_string(),
                }
            } else {
                let mut hinges: Vec<String> = Vec::new();
                let mut seen = HashSet::new();
                for s in [&thesis, &antithesis].into_iter().flatten() {
                    if let Some(node) = by_key.get(s.key.as_str()) {
                        if node.status != Status::In && seen.insert(node.because.clone()) {
                            hinges.push(format!("{}: {}", s.key, node.because));
                        }
                    }
                }
                if hinges.is_empty() {
                    "open; both sides stand — decided by a ruling, an observation or a synthesis"
                        .to_string()
                } else {
                    format!("open — decided by: {}", hinges.join("; "))
                }
            };
            Dispute {
                key: key.to_string(),
                state: state.clone(),
                thesis,
                antithesis,
                resolution,
                decided_by,
            }
        })
        .collect();

    Argument {
        nodes,
        disputes,
        warnings,
    }
}

fn premise_closure(premises: &[Vec<usize>], start: usize) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut stack = premises[start].clone();
    while let Some(p) = stack.pop() {
        if seen.insert(p) {
            stack.extend(premises[p].iter().copied());
        }
    }
    seen
}

/// One thing an agent can do to move a node out of `undecided` or `out`.
#[derive(Debug, Clone, Serialize)]
pub struct Move {
    pub key: String,
    pub what: String,
}

/// A cycle of mutual dependence among undecided nodes — the place where the
/// argument is genuinely stuck. Everything undecided is either in a root or
/// downstream of one.
#[derive(Debug, Clone, Serialize)]
pub struct Root {
    pub id: usize,
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disputes: Vec<String>,
    /// Other roots this one also hangs on (it resolves only after them).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hangs_on: Vec<usize>,
    pub moves: Vec<Move>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Downstream {
    pub key: String,
    pub roots: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Defeated {
    pub key: String,
    pub because: String,
    pub moves: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub key: String,
    pub dispute: String,
    pub what: String,
}

/// What would resolve the undecided and defeated claims: the root cycles and
/// the moves that break them, the claims downstream of each root, the
/// defeated claims and their reinstatement moves, and the hypotheses whose
/// dispute waits on an observation.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    pub roots: Vec<Root>,
    pub downstream: Vec<Downstream>,
    pub defeated: Vec<Defeated>,
    pub pending: Vec<Pending>,
}

impl Diagnosis {
    /// Keep only what concerns `keys`: roots with a selected member, and
    /// selected downstream/defeated/pending entries together with the roots
    /// they hang on.
    pub fn select(&mut self, keys: &HashSet<String>) {
        self.downstream.retain(|d| keys.contains(&d.key));
        self.defeated.retain(|d| keys.contains(&d.key));
        self.pending.retain(|p| keys.contains(&p.key));
        let mut keep: HashSet<usize> = self
            .downstream
            .iter()
            .flat_map(|d| d.roots.iter().copied())
            .collect();
        for root in &self.roots {
            if root.members.iter().any(|m| keys.contains(m)) {
                keep.insert(root.id);
            }
        }
        let mut grew = true;
        while grew {
            grew = false;
            for root in &self.roots {
                if keep.contains(&root.id) {
                    for h in &root.hangs_on {
                        if keep.insert(*h) {
                            grew = true;
                        }
                    }
                }
            }
        }
        self.roots.retain(|r| keep.contains(&r.id));
    }
}

/// Diagnose an argument: reduce every undecided node to the root cycle(s) it
/// hangs on, and name the move that would break each cycle.
pub fn diagnose(argument: &Argument) -> Diagnosis {
    let nodes = &argument.nodes;
    let position: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.key.as_str(), i))
        .collect();
    let n = nodes.len();

    // Dependency edges among undecided nodes: i hangs on each undecided
    // attacker and each undecided premise.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, node) in nodes.iter().enumerate() {
        if node.status != Status::Undecided {
            continue;
        }
        for a in &node.attackers {
            if a.status == Status::Undecided {
                if let Some(&j) = position.get(a.key.as_str()) {
                    deps[i].push(j);
                }
            }
        }
        for p in &node.premises {
            if p.status == Status::Undecided {
                if let Some(&j) = position.get(p.key.as_str()) {
                    deps[i].push(j);
                }
            }
        }
    }

    // Tarjan's SCC over the undecided subgraph.
    let mut index = 0usize;
    let mut idx: Vec<Option<usize>> = vec![None; n];
    let mut low: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut component: Vec<Option<usize>> = vec![None; n];
    let mut components: Vec<Vec<usize>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        v: usize,
        deps: &[Vec<usize>],
        index: &mut usize,
        idx: &mut [Option<usize>],
        low: &mut [usize],
        on_stack: &mut [bool],
        stack: &mut Vec<usize>,
        component: &mut [Option<usize>],
        components: &mut Vec<Vec<usize>>,
    ) {
        idx[v] = Some(*index);
        low[v] = *index;
        *index += 1;
        stack.push(v);
        on_stack[v] = true;
        for &w in &deps[v] {
            if idx[w].is_none() {
                strongconnect(
                    w, deps, index, idx, low, on_stack, stack, component, components,
                );
                low[v] = low[v].min(low[w]);
            } else if on_stack[w] {
                low[v] = low[v].min(idx[w].unwrap());
            }
        }
        if low[v] == idx[v].unwrap() {
            let mut members = Vec::new();
            while let Some(w) = stack.pop() {
                on_stack[w] = false;
                component[w] = Some(components.len());
                members.push(w);
                if w == v {
                    break;
                }
            }
            members.sort_unstable();
            components.push(members);
        }
    }
    for v in 0..n {
        if nodes[v].status == Status::Undecided && idx[v].is_none() {
            strongconnect(
                v,
                &deps,
                &mut index,
                &mut idx,
                &mut low,
                &mut on_stack,
                &mut stack,
                &mut component,
                &mut components,
            );
        }
    }

    // Roots are the non-trivial components, numbered in key order of their
    // first member.
    let mut root_components: Vec<usize> = (0..components.len())
        .filter(|c| components[*c].len() > 1)
        .collect();
    root_components.sort_by_key(|c| nodes[components[*c][0]].key.clone());
    let root_id: HashMap<usize, usize> = root_components
        .iter()
        .enumerate()
        .map(|(id, c)| (*c, id + 1))
        .collect();

    // Roots reachable from a node through undecided dependencies, excluding
    // the node's own component.
    let reachable_roots = |start: usize| -> Vec<usize> {
        let own = component[start];
        let mut seen = vec![false; n];
        let mut todo = vec![start];
        let mut found: Vec<usize> = Vec::new();
        while let Some(v) = todo.pop() {
            if seen[v] {
                continue;
            }
            seen[v] = true;
            if let Some(c) = component[v] {
                if Some(c) != own {
                    if let Some(&id) = root_id.get(&c) {
                        if !found.contains(&id) {
                            found.push(id);
                        }
                    }
                }
            }
            todo.extend(deps[v].iter().copied());
        }
        found.sort_unstable();
        found
    };

    let dispute_of = |key: &str| -> Option<(&Dispute, &str)> {
        argument.disputes.iter().find_map(|d| {
            let t = d.thesis.as_ref().map(|s| s.key.as_str());
            let a = d.antithesis.as_ref().map(|s| s.key.as_str());
            if t == Some(key) {
                Some((d, a.unwrap_or("")))
            } else if a == Some(key) {
                Some((d, t.unwrap_or("")))
            } else {
                None
            }
        })
    };

    let mut roots: Vec<Root> = Vec::new();
    for c in &root_components {
        let members = &components[*c];
        let in_cycle: HashSet<usize> = members.iter().copied().collect();
        let mut moves: Vec<Move> = Vec::new();
        let mut disputes: Vec<String> = Vec::new();
        for &i in members {
            let node = &nodes[i];
            if let Some((d, _)) = dispute_of(&node.key) {
                if !disputes.contains(&d.key) {
                    disputes.push(d.key.clone());
                }
            }
            if node.kind != "objection" {
                continue;
            }
            // The Against target of this objection, and the other side of the
            // dispute it enters, if any.
            let target = argument
                .nodes
                .iter()
                .find(|t| {
                    t.attackers
                        .iter()
                        .any(|a| a.key == node.key && a.kind != "undermines")
                })
                .map(|t| t.key.clone());
            let other_side = target
                .as_deref()
                .and_then(dispute_of)
                .map(|(d, other)| (d.key.clone(), other.to_string()));
            for p in &node.premises {
                let Some(&j) = position.get(p.key.as_str()) else {
                    continue;
                };
                if !in_cycle.contains(&j) {
                    continue;
                }
                let what = match (&target, &other_side) {
                    (Some(t), Some((d, other))) if other == &p.key => format!(
                        "circular ground: enters '{d}' against '{t}' and rests on the other side '{other}' — give it a ground outside the cycle, or answer it"
                    ),
                    _ => format!(
                        "rests on '{}' inside the cycle — give it a ground outside the cycle, or answer it",
                        p.key
                    ),
                };
                moves.push(Move {
                    key: node.key.clone(),
                    what,
                });
            }
            for a in &node.attackers {
                let Some(&j) = position.get(a.key.as_str()) else {
                    continue;
                };
                if !in_cycle.contains(&j) || nodes[j].kind != "objection" {
                    continue;
                }
                let mutual = nodes[j].attackers.iter().any(|b| b.key == node.key);
                if mutual && node.key < a.key {
                    moves.push(Move {
                        key: node.key.clone(),
                        what: format!(
                            "'{}' and '{}' attack each other — a reply is owed on one side",
                            node.key, a.key
                        ),
                    });
                }
            }
        }
        if moves.is_empty() {
            moves.push(Move {
                key: nodes[members[0]].key.clone(),
                what: "a cycle of attacks with no objection resting inside it — a reply is owed"
                    .to_string(),
            });
        }
        roots.push(Root {
            id: root_id[c],
            members: members.iter().map(|i| nodes[*i].key.clone()).collect(),
            disputes,
            hangs_on: reachable_roots(members[0]),
            moves,
        });
    }

    let mut downstream: Vec<Downstream> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if node.status != Status::Undecided {
            continue;
        }
        if component[i]
            .map(|c| components[c].len() > 1)
            .unwrap_or(false)
        {
            continue;
        }
        downstream.push(Downstream {
            key: node.key.clone(),
            roots: reachable_roots(i),
        });
    }

    let mut defeated: Vec<Defeated> = Vec::new();
    for node in nodes {
        if node.status != Status::Out || node.kind == "objection" {
            continue;
        }
        let mut moves = Vec::new();
        if let Some(a) = node.attackers.iter().find(|a| a.status == Status::In) {
            moves.push(format!(
                "answer '{}' (state: answered) — revise the claim to meet it",
                a.key
            ));
            moves.push("concede it and demote or delete the claim".to_string());
            moves.push(format!(
                "attack '{}' with a counter-objection grounded outside this dispute",
                a.key
            ));
        } else if let Some(p) = node.premises.iter().find(|p| p.status == Status::Out) {
            moves.push(format!("resolves with '{}'", p.key));
        }
        defeated.push(Defeated {
            key: node.key.clone(),
            because: node.because.clone(),
            moves,
        });
    }

    let mut pending: Vec<Pending> = Vec::new();
    for node in nodes {
        if node.kind != "hypothesis" {
            continue;
        }
        let settled = matches!(
            node.test_state.as_deref(),
            Some("supported") | Some("refuted")
        );
        if settled {
            continue;
        }
        if let Some((d, _)) = dispute_of(&node.key) {
            if d.state != "resolved" {
                pending.push(Pending {
                    key: node.key.clone(),
                    dispute: d.key.clone(),
                    what: "resolved by observation — the hypothesis's test has no Result"
                        .to_string(),
                });
            }
        }
    }

    Diagnosis {
        roots,
        downstream,
        defeated,
        pending,
    }
}

/// Text rendering of a diagnosis.
pub fn render_diagnosis_text(diagnosis: &Diagnosis) -> String {
    let mut out = String::new();
    out.push_str(&format!("roots ({}):\n", diagnosis.roots.len()));
    for root in &diagnosis.roots {
        out.push_str(&format!(
            "  #{}  cycle of {}: {}\n",
            root.id,
            root.members.len(),
            root.members.join(", ")
        ));
        if !root.disputes.is_empty() {
            out.push_str(&format!("      dispute: {}\n", root.disputes.join(", ")));
        }
        if !root.hangs_on.is_empty() {
            out.push_str(&format!(
                "      also hangs on: {}\n",
                root.hangs_on
                    .iter()
                    .map(|h| format!("#{h}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for m in &root.moves {
            out.push_str(&format!("      {}: {}\n", m.key, m.what));
        }
    }
    out.push_str(&format!("\ndownstream ({}):\n", diagnosis.downstream.len()));
    for d in &diagnosis.downstream {
        out.push_str(&format!(
            "  {}  ← {}\n",
            d.key,
            if d.roots.is_empty() {
                "(no root)".to_string()
            } else {
                d.roots
                    .iter()
                    .map(|r| format!("#{r}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    out.push_str(&format!("\ndefeated ({}):\n", diagnosis.defeated.len()));
    for d in &diagnosis.defeated {
        out.push_str(&format!("  {}  {}\n", d.key, d.because));
        for m in &d.moves {
            out.push_str(&format!("      {m}\n"));
        }
    }
    out.push_str(&format!("\npending ({}):\n", diagnosis.pending.len()));
    for p in &diagnosis.pending {
        out.push_str(&format!("  {}  in '{}': {}\n", p.key, p.dispute, p.what));
    }
    out
}

fn explain(status: Status, attackers: &[Attacker], premises: &[Premise]) -> String {
    match status {
        Status::In => {
            if attackers.is_empty() && premises.is_empty() {
                "unattacked".to_string()
            } else {
                let mut parts = Vec::new();
                if !attackers.is_empty() {
                    parts.push(format!(
                        "attackers out ({})",
                        attackers
                            .iter()
                            .map(|a| a.key.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !premises.is_empty() {
                    parts.push("premises in".to_string());
                }
                parts.join("; ")
            }
        }
        Status::Out => {
            if let Some(a) = attackers.iter().find(|a| a.status == Status::In) {
                format!("defeated by '{}' ({})", a.key, a.kind)
            } else if let Some(p) = premises.iter().find(|p| p.status == Status::Out) {
                format!("premise '{}' is out", p.key)
            } else {
                "out".to_string()
            }
        }
        Status::Undecided => {
            if let Some(a) = attackers.iter().find(|a| a.status == Status::Undecided) {
                format!("attacker '{}' ({}) is undecided", a.key, a.kind)
            } else if let Some(p) = premises.iter().find(|p| p.status == Status::Undecided) {
                format!("premise '{}' is undecided", p.key)
            } else {
                "undecided".to_string()
            }
        }
    }
}

/// Text rendering of an argument: one line per node, then the disputes and
/// warnings.
pub fn render_text(argument: &Argument) -> String {
    let mut out = String::new();
    let width = argument
        .nodes
        .iter()
        .map(|n| n.key.len())
        .max()
        .unwrap_or(0);
    for node in &argument.nodes {
        out.push_str(&format!(
            "{:width$}  {:9}  {}\n",
            node.key,
            node.status.as_str(),
            node.because,
            width = width
        ));
    }
    if !argument.disputes.is_empty() {
        out.push_str("\ndisputes:\n");
        for dispute in &argument.disputes {
            let side = |s: &Option<Side>| match s {
                Some(side) => format!(
                    "{} ({})",
                    side.key,
                    side.status.map(|s| s.as_str()).unwrap_or("not a claim")
                ),
                None => "—".to_string(),
            };
            out.push_str(&format!(
                "{}  [{}]  thesis {}  antithesis {}\n  {}\n",
                dispute.key,
                dispute.state,
                side(&dispute.thesis),
                side(&dispute.antithesis),
                dispute.decided_by
            ));
        }
    }
    if !argument.warnings.is_empty() {
        out.push_str("\nwarnings:\n");
        for warning in &argument.warnings {
            out.push_str(&format!("{}: {}\n", warning.key, warning.message));
        }
    }
    out
}
