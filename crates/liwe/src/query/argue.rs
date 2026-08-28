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
