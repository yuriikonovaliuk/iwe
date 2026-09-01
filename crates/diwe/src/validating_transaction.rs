//! T7: a [`Transaction`] backend that validates schema rules at `commit()`
//! time, scoped to an index-backed affected set.
//!
//! This is a NEW, non-default backend — [`liwe::transaction::NoopTransaction`]
//! stays the default, untouched write-permission passthrough every canonical
//! write path already routes through (T3/T5/T10/T11's job: freeze /
//! immutability, not schema shape). [`AffectedSetTransaction`] is a
//! different, separate mechanism: it does not check write permission at
//! all (every `write()` call succeeds), and instead checks, once at
//! `commit()`, whether the transaction's *final* state satisfies the
//! index-bounded schema rules over the documents that final state's writes
//! could have affected. See [`crate::schema::validate_affected_set`] for
//! the affected-set closure itself and the routing decision for the rule
//! forms that closure cannot bound.
//!
//! Validating only at `commit()`, and only the final state, is deliberate:
//! a multi-write transaction may legitimately pass through invalid
//! intermediate states on its way to a valid final one
//! (`m2/design-transactions`). This backend never inspects the state after
//! write 1 of a 2-write transaction — only the state after every write
//! recorded since `begin()` has been applied.

use std::fs;
use std::path::PathBuf;

use liwe::graph::Graph;
use liwe::model::config::Format;
use liwe::model::{Key, State};
use liwe::transaction::{CommitError, Transaction, Write, WriteRejected};

use crate::config::Configuration;
use crate::fs::{new_for_path, write_file};
use crate::schema::{validate_affected_set, ValidationRun};

/// Why an [`AffectedSetTransaction::commit`] failed, beyond the
/// [`CommitError::Failed`] state every [`Transaction`] shares.
#[derive(Debug)]
pub enum AffectedSetError {
    /// The schema configuration itself (a `.iwe/schemas/*.yaml` file, or
    /// the `[schemas]` bindings in `config.toml`) could not be compiled.
    Config(Vec<String>),
    /// The transaction's final state violates the index-bounded schema
    /// rules checked at commit. None of this transaction's writes were
    /// applied.
    Violations(ValidationRun),
    /// A filesystem failure while reading the current on-disk state or
    /// writing the transaction's changes.
    Io(std::io::Error),
}

/// A [`Transaction`] backend that performs schema validation at `commit()`
/// time, scoped to an index-backed affected set (T7).
///
/// `write()` always succeeds — this backend is not a permission gate.
/// `commit()` builds the state that would result from applying every write
/// recorded since `begin()` on top of what is currently on disk, computes
/// the affected set for the keys those writes touched (via
/// [`validate_affected_set`]), and checks only that bounded set of
/// documents. If it is clean, every pending write is applied to disk and
/// the transaction succeeds; if not, nothing is written and `commit()`
/// returns the violations.
pub struct AffectedSetTransaction {
    base_path: PathBuf,
    format: Format,
    config: Configuration,
    schemas_dir: PathBuf,
    pending: Vec<Write>,
    failed: bool,
}

impl AffectedSetTransaction {
    pub fn new(
        base_path: impl Into<PathBuf>,
        format: Format,
        config: Configuration,
        schemas_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            base_path: base_path.into(),
            format,
            config,
            schemas_dir: schemas_dir.into(),
            pending: Vec::new(),
            failed: false,
        }
    }

    /// The writes recorded on this transaction since the last `begin`,
    /// `commit`, or `abort`.
    pub fn pending(&self) -> &[Write] {
        &self.pending
    }

    /// The state that would result from applying every pending write on
    /// top of what is currently on disk at `base_path`.
    fn final_state(&self) -> State {
        let mut state = new_for_path(&self.base_path, self.format);
        for write in &self.pending {
            match write {
                Write::Put(key, content) => {
                    state.insert(key.as_str().to_string(), content.clone());
                }
                Write::Remove(key) => {
                    state.remove(key.as_str());
                }
            }
        }
        state
    }

    /// The distinct keys this transaction's pending writes name.
    fn touched_keys(&self) -> Vec<Key> {
        let mut keys: Vec<Key> = Vec::new();
        for write in &self.pending {
            let key = match write {
                Write::Put(key, _) => key.clone(),
                Write::Remove(key) => key.clone(),
            };
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    fn apply_pending(&self) -> std::io::Result<()> {
        for write in &self.pending {
            match write {
                Write::Put(key, content) => {
                    let file_path = self.base_path.join(key.to_path(self.format));
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    write_file(&key.as_str().to_string(), content, &self.base_path, self.format)?;
                }
                Write::Remove(key) => {
                    let file_path = self.base_path.join(key.to_path(self.format));
                    if file_path.exists() {
                        fs::remove_file(&file_path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Transaction for AffectedSetTransaction {
    type Error = AffectedSetError;

    fn begin(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.failed = false;
        Ok(())
    }

    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
        self.pending.push(write);
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
        if self.failed {
            return Err(CommitError::Failed);
        }

        let touched = self.touched_keys();
        if touched.is_empty() {
            self.pending.clear();
            return Ok(());
        }

        let state = self.final_state();
        let graph = Graph::from_state(
            &state,
            false,
            self.config.format_options(),
            self.config.library.frontmatter_document_title.clone(),
        );

        match validate_affected_set(&self.schemas_dir, &self.config, &graph, &touched) {
            Err(errors) => Err(CommitError::Other(AffectedSetError::Config(errors))),
            Ok((run, _affected)) if !run.reports.is_empty() => {
                Err(CommitError::Other(AffectedSetError::Violations(run)))
            }
            Ok(_) => match self.apply_pending() {
                Ok(()) => {
                    self.pending.clear();
                    Ok(())
                }
                Err(error) => Err(CommitError::Other(AffectedSetError::Io(error))),
            },
        }
    }

    fn abort(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.failed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::{create_dir_all, read_to_string, write};

    use tempfile::TempDir;

    use crate::config::{Patterns, SchemaBinding};

    fn write_schema(dir: &std::path::Path, name: &str, source: &str) {
        let schemas = dir.join(".iwe").join("schemas");
        create_dir_all(&schemas).unwrap();
        write(schemas.join(format!("{name}.yaml")), source).unwrap();
    }

    fn config_with(entries: &[(&str, &str)]) -> Configuration {
        Configuration {
            schemas: entries
                .iter()
                .map(|(name, pattern)| {
                    (
                        name.to_string(),
                        SchemaBinding {
                            r#match: Patterns::One(pattern.to_string()),
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn transaction_for(temp: &TempDir, config: Configuration) -> AffectedSetTransaction {
        AffectedSetTransaction::new(
            temp.path().to_path_buf(),
            Format::Markdown,
            config,
            temp.path().join(".iwe").join("schemas"),
        )
    }

    /// Test A: a two-write transaction whose intermediate state (after
    /// write 1) violates a schema rule, and whose final state (after write
    /// 2, same key) resolves it. `commit()` on the new backend succeeds
    /// because only the final state is checked. Then the SAME first write,
    /// committed alone as its own single-write transaction, is rejected by
    /// the same backend — the per-write-vs-at-commit difference made
    /// empirically visible, not just asserted in a comment.
    #[test]
    fn test_a_intermediate_state_permitted_same_write_alone_rejected() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("notes")).unwrap();
        write(temp.path().join("notes/b.md"), "# B\n").unwrap();

        let config = config_with(&[("note", "notes/**")]);
        let mut tx = transaction_for(&temp, config);

        let invalid_content = "# A\n\nno links here\n".to_string();
        let valid_content = "# A\n\nSee [B](b).\n".to_string();

        // --- Multi-write transaction: write 1 is individually invalid,
        // write 2 (same key) resolves it. Only the final state is checked.
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/a"), invalid_content.clone()))
            .unwrap();
        tx.write(Write::Put(Key::name("notes/a"), valid_content.clone()))
            .unwrap();
        let multi_write_result = tx.commit();
        println!("Test A, multi-write transaction commit result: {multi_write_result:?}");
        assert!(
            multi_write_result.is_ok(),
            "commit should succeed: the final state (write 2) is valid, even though the \
             intermediate state (write 1 alone) was not"
        );
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            valid_content,
            "the committed file should hold the final (write 2) content"
        );

        // --- Same first write, alone, as its own single-write transaction.
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/a"), invalid_content.clone()))
            .unwrap();
        let single_write_result = tx.commit();
        println!("Test A, single-write transaction (write 1 alone) commit result: {single_write_result:?}");
        assert!(
            matches!(single_write_result, Err(CommitError::Other(AffectedSetError::Violations(_)))),
            "committing write 1 alone should be rejected: for THIS transaction, write 1's \
             content is the final state, and it violates the min-links rule"
        );
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            valid_content,
            "the rejected commit must not have overwritten the file on disk"
        );
    }

    /// Test B: a transaction whose final state (after all its writes) is
    /// invalid is rejected at `commit()`, and none of its writes land on
    /// disk — verified by reading the filesystem, not just the return
    /// value.
    #[test]
    fn test_b_invalid_final_state_rejected_nothing_lands_on_disk() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("logs")).unwrap();

        let config = config_with(&[("note", "logs/**")]);
        let mut tx = transaction_for(&temp, config);

        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("logs/x"),
            "# X\n\nno links\n".to_string(),
        ))
        .unwrap();
        tx.write(Write::Put(
            Key::name("logs/y"),
            "# Y\n\nalso no links\n".to_string(),
        ))
        .unwrap();

        let result = tx.commit();
        println!("Test B, commit result: {result:?}");
        assert!(matches!(
            result,
            Err(CommitError::Other(AffectedSetError::Violations(_)))
        ));
        if let Err(CommitError::Other(AffectedSetError::Violations(run))) = &result {
            println!("Test B, violation reports: {:?}", run.reports);
            let keys: Vec<_> = run.reports.iter().map(|r| r.key.to_string()).collect();
            assert!(keys.contains(&"logs/x".to_string()));
            assert!(keys.contains(&"logs/y".to_string()));
        }

        assert!(
            !temp.path().join("logs/x.md").exists(),
            "logs/x.md must not have been written to disk"
        );
        assert!(
            !temp.path().join("logs/y.md").exists(),
            "logs/y.md must not have been written to disk"
        );
    }

    /// The direct-link (one-hop, RefIndex-backed) affected-set closure is
    /// real: removing a document that another document links to is
    /// rejected because the *referrer* (never itself touched) is pulled
    /// into the affected set and its "no such document" check fails.
    #[test]
    fn direct_link_closure_pulls_in_referrer_and_rejects_removal() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("hubs")).unwrap();
        create_dir_all(temp.path().join("assets")).unwrap();
        write(
            temp.path().join("hubs/hub.md"),
            "# Hub\n\nSee [Target](../assets/target).\n",
        )
        .unwrap();
        write(temp.path().join("assets/target.md"), "# Target\n").unwrap();

        // "assets/**" is deliberately not schema-bound: only "hubs/**" is.
        let config = config_with(&[("note", "hubs/**")]);
        let mut tx = transaction_for(&temp, config);

        tx.begin().unwrap();
        tx.write(Write::Remove(Key::name("assets/target"))).unwrap();
        let result = tx.commit();
        println!("Direct-link closure test, commit result: {result:?}");

        match &result {
            Err(CommitError::Other(AffectedSetError::Violations(run))) => {
                println!("Direct-link closure test, violation reports: {:?}", run.reports);
                assert!(
                    run.reports.iter().any(|r| r.key == Key::name("hubs/hub")),
                    "hubs/hub was never touched by this transaction, but it links to the \
                     removed key — it should have been pulled into the affected set via \
                     RefIndex and rejected for a dangling link"
                );
            }
            other => panic!("expected the removal to be rejected, got: {other:?}"),
        }
        assert!(
            temp.path().join("assets/target.md").exists(),
            "the rejected commit must not have removed the file on disk"
        );
    }

    /// The `reach`-rule (transitive-to-fixpoint, ViaWalk::inbound-backed)
    /// affected-set closure is real: breaking a link in the middle of a
    /// genus chain is rejected because a document further up the chain
    /// (never itself touched) is pulled into the affected set and its
    /// `reach` check fails.
    #[test]
    fn reach_closure_pulls_in_transitive_referrer_and_rejects_broken_chain() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "concept",
            "links:\n  - within: Is a\n    min: 1\n    max: 1\n    reach: root/entity\n",
        );
        create_dir_all(temp.path().join("concepts")).unwrap();
        create_dir_all(temp.path().join("root")).unwrap();
        write(temp.path().join("root/entity.md"), "# Entity\n").unwrap();
        write(
            temp.path().join("concepts/mid.md"),
            "# Mid\n\n## Is a\n\n- [Entity](../root/entity)\n",
        )
        .unwrap();
        write(
            temp.path().join("concepts/leaf.md"),
            "# Leaf\n\n## Is a\n\n- [Mid](mid)\n",
        )
        .unwrap();

        let config = config_with(&[("concept", "concepts/**")]);
        let mut tx = transaction_for(&temp, config);

        // Redirect mid's genus link away from entity, breaking the chain
        // both for mid itself and for leaf, which reaches entity only
        // through mid.
        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("concepts/mid"),
            "# Mid\n\n## Is a\n\n- [Stub](stub)\n".to_string(),
        ))
        .unwrap();
        let result = tx.commit();
        println!("Reach closure test, commit result: {result:?}");

        match &result {
            Err(CommitError::Other(AffectedSetError::Violations(run))) => {
                println!("Reach closure test, violation reports: {:?}", run.reports);
                assert!(
                    run.reports.iter().any(|r| r.key == Key::name("concepts/leaf")),
                    "concepts/leaf was never touched by this transaction, but it reaches the \
                     touched key through the 'Is a' scope — it should have been pulled into \
                     the affected set via ViaWalk::inbound and rejected for a broken chain"
                );
            }
            other => panic!("expected the broken chain to be rejected, got: {other:?}"),
        }
        assert_eq!(
            read_to_string(temp.path().join("concepts/mid.md")).unwrap(),
            "# Mid\n\n## Is a\n\n- [Entity](../root/entity)\n",
            "the rejected commit must not have overwritten the file on disk"
        );
    }
}
