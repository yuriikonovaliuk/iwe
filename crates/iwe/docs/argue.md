# Dialectic — `iwe argue`

`iwe argue` computes the *standing* of every claim in the store from the
objections raised against it and the premises it rests on. It is purely
dialectical reasoning — arguments between propositions — never inference:
it derives no new claims, deduces nothing from definitions, and updates no
beliefs. It is a reading, not a gate: the command always exits 0.

## Nodes and edges

- **Nodes** are the claim-bearing documents — `type` one of `fact`,
  `pattern`, `model`, `stance`, `conjecture`, `hypothesis` — and every
  `objection`.
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
that attacks or supports it.
