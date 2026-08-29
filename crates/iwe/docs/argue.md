# Dialectic — `iwe argue`

`iwe argue` computes the *standing* of every claim in the store from the
objections raised against it and the premises it rests on. It is purely
dialectical reasoning — arguments between propositions — never inference:
it derives no new claims, deduces nothing from definitions, and updates no
beliefs. It is a reading, not a gate: the command always exits 0.

## Nodes and edges

- **Nodes** are the claim-bearing documents — `type` one of `fact`,
  `pattern`, `model`, `stance`, `conjecture`, `hypothesis` — every
  `objection`, and every `axiom`. An axiom rests on nothing and cannot be
  attacked: an objection against one is reported and ignored. It is the
  floor a chain of support ends on.
- **Attack edges** come from objections. An objection attacks the document
  linked in its `## Against` section. When its `kind` is `undermines` and it
  names a premise in `## Undermines`, the attack lands on that premise
  instead — the claim falls, if it falls, through support.
- **Support edges** are `## Rests on` links between nodes. Links to anything
  that is not a node (a concept, an observation, a ruling, a dispute) are
  neither support nor attack.
- An objection's `state` decides whether it counts: `open` — live;
  `answered` — attacks nothing (the claim was revised to meet it);
  `conceded` — stands, and the claim should have been demoted or deleted; if
  it is still present `argue` says so under `warnings`.

The three objection kinds are the three places an argument can be hit:
`rebuts` the conclusion, `undercuts` the inference from grounds to
conclusion (the grounds stand), `undermines` a premise.

**Quantity.** When documents carry `quantity: universal | generic |
particular`, a *particular* objection against a *generic* claim is not an
attack: a generalisation is not overturned by an edge case. The objection
is reported under `warnings` as an exception — the claim's scope should
absorb it and the objection be answered — and the claim's standing is
unchanged. Against a universal, a particular is a counter-instance and
defeats it. Documents without `quantity` attack and are attacked as
before.

## Propositions, contrariety and strength

A claim or objection may carry a structured **proposition** in its
frontmatter — `proposition: { subject, predicate, object, polarity }`,
polarity `affirm` (default) or `deny`, compared case-insensitively. When
both a rebutting or undermining objection and its target carry one, the
attack counts only if the objection's proposition is a *contrary* of the
target's: same subject, predicate and object, opposite polarity. A
non-contrary is reported (`not an attack: …`) and ignored — an attack
whose conclusion does not contradict its target is not an attack. When
only one side is structured the attack is trusted and reported as
`contrariety not checkable`; when neither is, it is trusted silently.

Attacks land; **defeats** succeed. An undercut always defeats. A rebuttal
or undermining defeats only when the attacker is not weaker than its
target, by *weakest-link strength*: a node's own base, bounded by the
least of its premises. Bases: axiom 5; fact by warrant — observed 4,
established 3, authority 2, inference unbounded; pattern, model, stance
3; conjecture and hypothesis by confidence — high 2, else 1; objection
unbounded (its grounds decide). A `strength: N` field overrides the base.
So an objection resting on a conjecture cannot defeat an established
fact, and nothing is weaker than an observation but an axiom. A landed
attack that fails appears in `because` as `attacks fail, weaker (…)`.

A proposition argued for by several documents is a **conclusion**,
justified when any argument for it is in (`conclusions` in the output).

## Standing

The grounded extension — the unique, sceptical one — extended with
deductive support, iterated to a fixpoint:

- a node is **in** when every attacker is out and every premise is in
  (an unattacked, premise-free node is in);
- a node is **out** when some attacker is in or some premise is out;
- everything else stays **undecided**.

Nothing is accepted that the argument does not force: a symmetric standoff
(two claims each undermining the other's ground), an odd cycle of
objections, or a claim resting on an undecided premise all remain
undecided. A reply that defeats an objection reinstates its target; a
defeated premise takes every claim resting on it down, however many.

## Output

Text, one line per node — key, status, and why: `unattacked`, `attackers
out (…)`, `defeated by 'K' (kind)`, `premise 'P' is out`, `attacker 'K'
(kind) is undecided`, `premise 'P' is undecided` — followed by the
disputes (state, thesis and antithesis with their status, and what decides
it: the resolution, or for an open dispute the attacker or premise each side
hangs on) and warnings.

```text
world/engineering/stances/code-is-the-liability  undecided  attacker 'world/engineering/objections/2026-08-29-fewer-lines' (undermines) is undecided
```

JSON (`-f json`): `{ nodes, disputes, warnings }`; each node carries
`type`, `state` (objections), `status`, `attackers` (key, kind, status),
`premises` (key, status) and `because`.

`-k KEY` and `--filter EXPR` restrict what is printed; standing is always
computed over the whole store, since a node's status depends on everything
that attacks or supports it. The same standing is available to every filter
as `$standing: in | out | undecided` (`iwe docs query`), so `find`, schema
`links` rules and `[invariants]` can select on it.

## Warnings

Warnings are shapes the argument cannot resolve on its own:

- `conceded but not demoted: 'K' still stands in the graph` — a conceded
  objection whose target is still present;
- `objection attacks 'K', which no longer exists` / `…which is not a claim
  or objection` / `objection attacks nothing`;
- `circular ground: rests on 'K', the claim it attacks` — an objection
  whose premises (transitively) include its own target;
- `denies nothing in 'K': the quoted sentence is not in it` — a `rebuts`
  or `undermines` objection with a `## Denies` section must quote a
  sentence of its target (link and emphasis markup and whitespace are
  ignored); an attack whose conclusion contradicts nothing its target
  says is not an attack (the contrariness requirement of structured
  argumentation);
- `support cycle: A → B` — a chain of `Rests on` that returns to itself
  never reaches the floor; everything on it stays undecided;
- `an axiom cannot be attacked: 'K'`;
- `circular ground: enters 'D' against 'T' and rests on the other side 'A'`
  — an objection against one side of a dispute whose premises include the
  other side. Two of these facing each other are a Nixon diamond: both
  sides stay undecided, and nothing but a new ground or an answer moves
  them. The direct case is best made uncommittable by a `links` rule on the
  objection's `Rests on` (`iwe docs schema` §11); this warning also catches
  the transitive case.

## Diagnosis — `--explain`

`iwe argue --explain` reduces what is undecided or defeated to what would
move it:

- **roots** — the strongly connected components of mutual dependence among
  undecided nodes (an undecided node hangs on its undecided attackers and
  premises). Every undecided node is in a root or downstream of one. Each
  root lists its members, the dispute(s) its members sit in, other roots it
  also hangs on, and its *moves*: for each objection in the cycle, the
  premise inside the cycle it should not rest on (`circular ground: …` when
  that premise is the other side of the dispute it enters — give it a
  ground outside the cycle, or answer it), or the mutual attack on which a
  reply is owed;
- **downstream** — undecided nodes that are not in any cycle, with the
  root(s) they resolve with;
- **defeated** — claims that are out, with why and the reinstatement moves:
  answer the defeater (state `answered`), concede and demote, or counter it
  with an objection grounded outside the dispute; a claim out through a
  premise resolves with that premise;
- **pending** — hypotheses without a `supported`/`refuted` test result that
  are a side of an unresolved dispute: resolved by observation, not
  argument.

The diagnosis names the *slot* — which objection needs an independent
ground, which hypothesis needs its result — never the content that fills it.
`-k`/`--filter` keep the roots with a selected member and the selected
downstream, defeated and pending entries together with the roots they hang
on. `-f json` gives `{ roots, downstream, defeated, pending }`.
