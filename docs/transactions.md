# Transactions

Every write IWE makes to a document store — CLI `create`, `new`, `update`,
`delete`, `rename`, `extract`, `inline`, `attach`, `normalize`, and their
MCP equivalents (`iwe_create`, `iwe_update`, `iwe_delete`, `iwe_query`'s
`update` / `delete`, `iwe_rename`, `iwe_extract`, `iwe_inline`,
`iwe_attach`) — is routed through a transaction: a `begin` / `write` /
`commit` (or `abort`) cycle, rather than writing files directly.

## Why this matters

A single-document operation is already effectively atomic — one file
either has its new content or its old content. Transactions matter most
for **multi-write** operations: a bulk `update` touching many documents, or
a `delete` that also has to clean up references in documents that link to
the deleted one. Without a transaction, a crash or a rejected write partway
through such a batch could leave some documents rewritten and others not —
a store half-migrated, with no clean way to tell which half.

With a transaction:

- **`begin`** opens a fresh batch. Nothing is written yet.
- **`write`** records one change (create/overwrite a document, or remove
  one) into that batch.
- **`commit`** makes every write recorded since `begin` durable, together.
  If commit itself fails (or the transaction already failed — see
  "Rejected writes fail the whole transaction" below), *nothing* from this
  batch is written.
- **`abort`** discards every write recorded since `begin`, unconditionally
  — this is always available, even from a failed transaction, which is why
  abort (not commit) is the way out of one.

`Transaction` is a storage-agnostic interface — it says nothing about
files, directories, or version control. A backend implements it however it
holds its documents, as long as it can accept a write and later either make
every recorded write durable (`commit`) or discard them (`abort`); nothing
in the interface assumes a filesystem is involved, so a third-party backend
(an in-memory store, a database, a remote API) can implement it without
knowing why IWE wanted a transaction boundary in the first place.

## Commit validates the final state, not each write along the way

A transaction that records several writes is validated once, at `commit`
— against the state that would result from *all* of them, not
write-by-write as they are recorded. This is deliberate: a multi-write
transaction may legitimately pass through an intermediate state that would
itself be invalid on its way to a valid final one.

For example, renaming a required section in two documents that reference
each other by that section's name is naturally a two-write operation:
after the first write, one document may momentarily reference a section
name the other document no longer has — a violation, if it were checked
right then. After the second write, both documents agree again, and the
transaction as a whole is valid. Checking only the final state is what
lets this kind of batch succeed; checking after every individual write
would reject it despite the end result being entirely correct.

## Backends

Two backends exist:

- **The default** records writes and lets them through unchanged; the
  actual file writing happens exactly as it always did, through the write
  path surrounding it. Using the default changes nothing about how a write
  behaves today — it is today's behavior in today's mode, now expressed
  through the transaction interface uniformly, so every write path (CLI
  and MCP alike) goes through one mechanism rather than each reimplementing
  its own commit/rollback handling.
- **An affected-set-validating backend** does real storage: at `commit`,
  it builds the on-disk state that every recorded write would produce,
  checks the schema rules bound to the documents that state's writes could
  have affected, and — only if that check is clean — applies every pending
  write to disk together. If the check finds a violation, or the schema
  configuration itself fails to load, nothing is written and the
  violations are reported back. This backend is available in the
  codebase; it is not yet the one wired into the shipped CLI/MCP write
  paths by default.

Both backends honor the same failed-transaction contract described next.

## Rejected writes fail the whole transaction

If any single write in a transaction is rejected for lack of permission —
for example because the target document is frozen, or one of its
properties is marked immutable (see [Document Schema](document-schema.md),
"Freeze" and "Per-property mutability") — the transaction moves into a
failed state:

- From a failed state, only `abort` is permitted.
- From a failed state, `commit` must refuse rather than attempt to persist
  a partial or unauthorized set of writes.

A rejection never lets the writes recorded before it land while the
rejected one is silently dropped — the only way out of a failed
transaction is `abort`, which discards everything recorded on it.

## See also

- [Document Schema](document-schema.md) — freeze and per-property
  mutability, the two ways a `write` can be rejected for lack of
  permission.
- [Query Language Specification §10.2](spec.md#102-across-document) — the
  query engine's own atomicity guarantees are per-document; this is the
  mechanism the host (CLI/MCP) uses underneath to sequence writes reaching
  storage.
- [Query Language: Validation and atomicity](query-language.md#validation-and-atomicity)
  — the same all-or-nothing guarantee, described at the level of one
  `update` operation's frontmatter and block edits.
