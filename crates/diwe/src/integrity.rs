//! Structural link integrity (`[integrity]`): broken links and orphans,
//! enforced at the one commit point every write goes through and reported
//! by `iwe schema validate` — the same definitions and the same code, so
//! the commit gate and a whole-store validation (kc's drain) never
//! disagree.
//!
//! - **Broken link**: an internal link or inclusion whose target key has
//!   no document — exactly [`crate::stats::broken_links`]. External URLs
//!   (anything with a URI scheme) are not internal links. A fragment is
//!   dropped when the target key is resolved, so a link to a missing
//!   anchor inside an existing document is not broken; a bare `#anchor`
//!   link is not a document link at all.
//! - **Orphan**: a document not reachable from the root key (`index` by
//!   default) by following outgoing links and inclusions. Islands of pages
//!   that only link to each other are orphans. The root itself is never
//!   one; when the root document does not exist, every document is.
//!
//! Per property, `off` enforces nothing, `no-new` refuses a commit that
//! adds a violation the pre-commit state did not have (a broken link is
//! identified by its source and target keys, an orphan by its key), and
//! `strict` refuses a commit that leaves any violation.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};

use liwe::graph::Graph;
use liwe::model::config::{FormatOptions, RefsPath};
use liwe::model::{Key, State};
use liwe::schema::Violation;

use crate::config::{IntegrityMode, IntegrityOptions};
use crate::schema::KeyReport;
use crate::stats::{broken_links, BrokenLink};

/// The MCP error data code (`iwe_error`) a refused write carries.
pub const ERROR_CODE: &str = "link_integrity";

/// Every refusal's rendered text starts with this, wherever it is wrapped.
pub const ERROR_PREFIX: &str = "link integrity:";

/// The `schema` field of the reports `iwe schema validate` emits.
pub const REPORT_SCHEMA: &str = "integrity";

/// How to repair a refused write.
pub const HINT: &str = "pass link_from=<a reachable parent key> to iwe_create, or create the page and its link in one transaction (iwe_tx_begin … iwe_tx_commit), or link it from a reachable page in the same write";

const BROKEN_HINT: &str = "fix or remove the link, or create the target page in the same transaction";

/// How many broken links and orphans a refusal lists before it counts the rest.
const LISTED_CAP: usize = 20;

/// Whether `message` is (or wraps) an integrity refusal.
pub fn is_integrity_error(message: &str) -> bool {
    message.contains(ERROR_PREFIX)
}

/// The document keys reachable from `root` by following outgoing links and
/// inclusions, `root` included. Empty when `root` has no document.
pub fn reachable_from(graph: &Graph, root: &Key) -> HashSet<Key> {
    let mut seen: HashSet<Key> = HashSet::new();
    if !graph.has_key(root) {
        return seen;
    }
    let mut queue: VecDeque<Key> = VecDeque::new();
    seen.insert(root.clone());
    queue.push_back(root.clone());
    while let Some(key) = queue.pop_front() {
        let inclusions = graph
            .get_inclusion_edges_in(&key)
            .into_iter()
            .filter_map(|id| graph.graph_node(id).ref_key());
        for target in inclusions.chain(graph.get_reference_edges_in(&key)) {
            if !seen.contains(&target) && graph.has_key(&target) {
                seen.insert(target.clone());
                queue.push_back(target);
            }
        }
    }
    seen
}

/// Every document not reachable from `root`, sorted.
pub fn unreachable_keys(graph: &Graph, root: &Key) -> Vec<Key> {
    let reachable = reachable_from(graph, root);
    let mut keys: Vec<Key> = graph
        .keys()
        .into_iter()
        .filter(|key| !reachable.contains(key))
        .collect();
    keys.sort();
    keys
}

/// A store's integrity debt: its broken links and its orphans.
#[derive(Debug, Clone, Default)]
pub struct Debt {
    pub broken_links: Vec<BrokenLink>,
    pub orphans: Vec<Key>,
}

impl Debt {
    pub fn is_empty(&self) -> bool {
        self.broken_links.is_empty() && self.orphans.is_empty()
    }
}

/// Both kinds of debt, whatever the configured modes — what stats report.
pub fn full_debt(graph: &Graph, root: &str) -> Debt {
    Debt {
        broken_links: broken_links(graph),
        orphans: unreachable_keys(graph, &Key::name(root)),
    }
}

/// The debt the configured modes look at: a property left `off` is not
/// computed and stays empty.
pub fn debt(graph: &Graph, options: &IntegrityOptions) -> Debt {
    Debt {
        broken_links: if options.links.is_off() {
            Vec::new()
        } else {
            broken_links(graph)
        },
        orphans: if options.orphans.is_off() {
            Vec::new()
        } else {
            unreachable_keys(graph, &Key::name(&options.root))
        },
    }
}

/// A commit refused for its integrity: what it would add (`no-new`) or
/// leave (`strict`).
#[derive(Debug, Clone)]
pub struct IntegrityViolation {
    pub links: IntegrityMode,
    pub orphans: IntegrityMode,
    pub root: String,
    pub broken_links: Vec<BrokenLink>,
    pub orphan_keys: Vec<Key>,
}

fn link_pair(link: &BrokenLink) -> (Key, Key) {
    (link.source_key.clone(), link.target_key.clone())
}

/// The commit gate. `after` is the state the commit would produce;
/// `before` computes the pre-commit state's [`debt`], and is called only
/// when a `no-new` property has something to compare. On success, returns
/// the post-commit state's debt (what the next commit's `before` is, once
/// this one lands — see [`DebtCache`]).
pub fn check_commit(
    options: &IntegrityOptions,
    after: &Graph,
    before: impl FnOnce() -> Debt,
) -> Result<Debt, IntegrityViolation> {
    if !options.is_enabled() {
        return Ok(Debt::default());
    }
    let after = debt(after, options);
    let accepted = after.clone();
    let needs_before = (options.links == IntegrityMode::NoNew && !after.broken_links.is_empty())
        || (options.orphans == IntegrityMode::NoNew && !after.orphans.is_empty());
    let before = if needs_before { before() } else { Debt::default() };

    let broken: Vec<BrokenLink> = match options.links {
        IntegrityMode::Off => Vec::new(),
        IntegrityMode::Strict => after.broken_links,
        IntegrityMode::NoNew => {
            let standing: HashSet<(Key, Key)> = before.broken_links.iter().map(link_pair).collect();
            after
                .broken_links
                .into_iter()
                .filter(|link| !standing.contains(&link_pair(link)))
                .collect()
        }
    };
    let orphans: Vec<Key> = match options.orphans {
        IntegrityMode::Off => Vec::new(),
        IntegrityMode::Strict => after.orphans,
        IntegrityMode::NoNew => {
            let standing: HashSet<&Key> = before.orphans.iter().collect();
            after
                .orphans
                .into_iter()
                .filter(|key| !standing.contains(key))
                .collect()
        }
    };
    if broken.is_empty() && orphans.is_empty() {
        return Ok(accepted);
    }
    Err(IntegrityViolation {
        links: options.links,
        orphans: options.orphans,
        root: options.root.clone(),
        broken_links: broken,
        orphan_keys: orphans,
    })
}

/// A 64-bit fingerprint of a whole store state plus whatever else its
/// [`debt`] depends on (`context`: the parse options and the
/// `[integrity]` section) — the key [`DebtCache`] is looked up by.
pub fn state_digest(state: &State, context: &str) -> u64 {
    let mut keys: Vec<&String> = state.keys().collect();
    keys.sort_unstable();
    let mut hasher = rustc_hash::FxHasher::default();
    context.hash(&mut hasher);
    for key in keys {
        key.hash(&mut hasher);
        state[key].hash(&mut hasher);
    }
    hasher.finish()
}

/// The debt of the last store state a commit landed (or computed), keyed
/// by [`state_digest`]. Under `no-new` every commit compares against the
/// pre-commit state's debt; in a long-lived process (the iwec daemon) that
/// state is almost always exactly the one the previous commit produced,
/// so its debt is looked up instead of parsing the whole store again. The
/// key is the digest of what is actually read from disk, so a write from
/// anywhere else is simply a miss.
pub struct DebtCache;

static DEBT_CACHE: std::sync::Mutex<Option<(u64, Debt)>> = std::sync::Mutex::new(None);

impl DebtCache {
    pub fn get(digest: u64) -> Option<Debt> {
        let cache = DEBT_CACHE.lock().ok()?;
        match cache.as_ref() {
            Some((key, debt)) if *key == digest => Some(debt.clone()),
            _ => None,
        }
    }

    pub fn put(digest: u64, debt: Debt) {
        if let Ok(mut cache) = DEBT_CACHE.lock() {
            *cache = Some((digest, debt));
        }
    }
}

fn qualifier(mode: IntegrityMode) -> &'static str {
    match mode {
        IntegrityMode::NoNew => "new ",
        _ => "",
    }
}

impl fmt::Display for IntegrityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut counts = Vec::new();
        if !self.broken_links.is_empty() {
            counts.push(format!(
                "{} {}broken link(s)",
                self.broken_links.len(),
                qualifier(self.links)
            ));
        }
        if !self.orphan_keys.is_empty() {
            counts.push(format!(
                "{} {}orphan(s)",
                self.orphan_keys.len(),
                qualifier(self.orphans)
            ));
        }
        write!(
            f,
            "{ERROR_PREFIX} refused, the write would leave {} (links = \"{}\", orphans = \"{}\", root = \"{}\"); nothing was written",
            counts.join(" and "),
            self.links.label(),
            self.orphans.label(),
            self.root
        )?;
        for link in self.broken_links.iter().take(LISTED_CAP) {
            write!(f, "\n  broken link: {} → {}", link.source_key, link.target_key)?;
        }
        if self.broken_links.len() > LISTED_CAP {
            write!(
                f,
                "\n  … and {} more broken link(s)",
                self.broken_links.len() - LISTED_CAP
            )?;
        }
        for key in self.orphan_keys.iter().take(LISTED_CAP) {
            write!(f, "\n  orphan: {key} (not reachable from '{}')", self.root)?;
        }
        if self.orphan_keys.len() > LISTED_CAP {
            write!(
                f,
                "\n  … and {} more orphan(s)",
                self.orphan_keys.len() - LISTED_CAP
            )?;
        }
        if !self.broken_links.is_empty() {
            write!(f, "\nhint (broken link): {BROKEN_HINT}")?;
        }
        if !self.orphan_keys.is_empty() {
            write!(f, "\nhint (orphan): {HINT}")?;
        }
        Ok(())
    }
}

impl std::error::Error for IntegrityViolation {}

/// What `iwe schema validate` reports for integrity: `failing` for every
/// property in `strict` mode, `warnings` (current debt, not a failure) for
/// every property in `no-new` mode — a whole-store validation has no
/// pre-commit state to tell new debt from old, and `no-new` debt that
/// stood before a write is not that write's to pay. `selection`, when
/// given, keeps only the reports keyed to those documents (a broken link
/// is keyed to its source, an orphan to itself), which is how kc
/// attributes a report to a pending write.
#[derive(Debug, Default)]
pub struct IntegrityReports {
    pub failing: Vec<KeyReport>,
    pub warnings: Vec<KeyReport>,
}

pub fn validation_reports(
    options: &IntegrityOptions,
    graph: &Graph,
    selection: Option<&HashSet<Key>>,
) -> IntegrityReports {
    let mut reports = IntegrityReports::default();
    if !options.is_enabled() {
        return reports;
    }
    let debt = debt(graph, options);
    let selected = |key: &Key| selection.is_none_or(|keys| keys.contains(key));

    let mut by_source: BTreeMap<Key, Vec<Violation>> = BTreeMap::new();
    for link in debt.broken_links.iter().filter(|link| selected(&link.source_key)) {
        by_source
            .entry(link.source_key.clone())
            .or_default()
            .push(Violation {
                breadcrumb: Vec::new(),
                message: format!("broken link: '{}' does not exist", link.target_key),
                hint: Some(BROKEN_HINT.to_string()),
                schema_pointer: "/integrity/links".to_string(),
                keyword: "broken-link".to_string(),
            });
    }
    let link_reports = by_source.into_iter().map(|(key, violations)| KeyReport {
        key,
        schema: REPORT_SCHEMA.to_string(),
        violations,
    });
    match options.links {
        IntegrityMode::Strict => reports.failing.extend(link_reports),
        IntegrityMode::NoNew => reports.warnings.extend(link_reports),
        IntegrityMode::Off => {}
    }

    let orphan_reports = debt
        .orphans
        .into_iter()
        .filter(|key| selected(key))
        .map(|key| KeyReport {
            key,
            schema: REPORT_SCHEMA.to_string(),
            violations: vec![Violation {
                breadcrumb: Vec::new(),
                message: format!("orphan: not reachable from '{}'", options.root),
                hint: Some(HINT.to_string()),
                schema_pointer: "/integrity/orphans".to_string(),
                keyword: "orphan".to_string(),
            }],
        });
    match options.orphans {
        IntegrityMode::Strict => reports.failing.extend(orphan_reports),
        IntegrityMode::NoNew => reports.warnings.extend(orphan_reports),
        IntegrityMode::Off => {}
    }
    reports
}

/// The title a new document's link should carry: its ref text as the
/// graph derives it (heading, or the configured frontmatter title), else
/// its key.
pub fn document_title(
    key: &Key,
    content: &str,
    format_options: FormatOptions,
    frontmatter_document_title: Option<String>,
) -> String {
    let mut state = State::new();
    state.insert(key.as_str().to_string(), content.to_string());
    let graph = Graph::from_state(&state, false, format_options, frontmatter_document_title);
    graph
        .get_key_title(key)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| key.to_string())
}

/// `parent_content` with a list item linking `child` appended at its end —
/// `- [<title>](<url>)`, the URL written the way the store writes links
/// (`refs_path` relative to the parent's directory or absolute, plus
/// `refs_extension`). Joins a list that already ends the document;
/// otherwise starts one after a blank line.
pub fn append_link_item(
    parent_content: &str,
    parent: &Key,
    child: &Key,
    title: &str,
    format_options: &FormatOptions,
) -> String {
    let mut url = match format_options.refs_path() {
        RefsPath::Relative => child.to_rel_link_url(&parent.parent()),
        RefsPath::Absolute => format!("/{}", child.to_library_url()),
    };
    url.push_str(format_options.refs_extension());
    let text = title.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]");
    let item = format!("- [{text}]({url})\n");

    let body = parent_content.trim_end_matches(['\n', '\r', ' ', '\t']);
    if body.is_empty() {
        return item;
    }
    let last_line = body.lines().last().unwrap_or_default().trim_start();
    let joins_list = ["- ", "* ", "+ "].iter().any(|marker| last_line.starts_with(marker));
    if joins_list {
        format!("{body}\n{item}")
    } else {
        format!("{body}\n\n{item}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(docs: &[(&str, &str)]) -> Graph {
        let mut state = State::new();
        for (key, content) in docs {
            state.insert(key.to_string(), content.to_string());
        }
        Graph::from_state(&state, false, FormatOptions::default(), None)
    }

    fn opts(links: IntegrityMode, orphans: IntegrityMode) -> IntegrityOptions {
        IntegrityOptions {
            links,
            orphans,
            root: "index".to_string(),
        }
    }

    #[test]
    fn reachability_follows_links_and_inclusions_and_islands_are_orphans() {
        let g = graph(&[
            ("index", "# Index\n\n[A](a)\n"),
            ("a", "# A\n\nSee [B](b) inline.\n"),
            ("b", "# B\n"),
            ("island1", "# I1\n\n[I2](island2)\n"),
            ("island2", "# I2\n\n[I1](island1)\n"),
        ]);
        assert_eq!(
            unreachable_keys(&g, &Key::name("index")),
            vec![Key::name("island1"), Key::name("island2")]
        );
    }

    #[test]
    fn missing_root_orphans_everything() {
        let g = graph(&[("a", "# A\n")]);
        assert_eq!(unreachable_keys(&g, &Key::name("index")), vec![Key::name("a")]);
    }

    #[test]
    fn anchors_and_external_urls_are_not_broken() {
        let g = graph(&[
            ("index", "# Index\n\n[A](a)\n"),
            ("a", "# A\n\n[x](https://example.com) [y](index#nowhere) [z](#local)\n"),
        ]);
        assert!(debt(&g, &opts(IntegrityMode::Strict, IntegrityMode::Strict)).is_empty());
    }

    #[test]
    fn no_new_tolerates_standing_debt_and_refuses_growth() {
        let options = opts(IntegrityMode::NoNew, IntegrityMode::NoNew);
        let before = graph(&[("index", "# Index\n"), ("old", "# Old\n\n[gone](gone)\n")]);
        let same = graph(&[("index", "# Index\n"), ("old", "# Old v2\n\n[gone](gone)\n")]);
        assert!(check_commit(&options, &same, || debt(&before, &options)).is_ok());

        let grown = graph(&[
            ("index", "# Index\n"),
            ("old", "# Old\n\n[gone](gone)\n"),
            ("new", "# New\n\n[m](missing)\n"),
        ]);
        let refused = check_commit(&options, &grown, || debt(&before, &options)).unwrap_err();
        assert_eq!(refused.orphan_keys, vec![Key::name("new")]);
        assert_eq!(refused.broken_links.len(), 1);
        let text = refused.to_string();
        assert!(text.starts_with(ERROR_PREFIX), "{text}");
        assert!(text.contains("new → missing"), "{text}");
        assert!(text.contains("iwe_tx_begin … iwe_tx_commit"), "{text}");
        assert!(text.contains("link_from="), "{text}");
    }

    #[test]
    fn strict_refuses_any_debt() {
        let options = opts(IntegrityMode::Strict, IntegrityMode::Strict);
        let g = graph(&[("index", "# Index\n"), ("old", "# Old\n")]);
        let refused = check_commit(&options, &g, || panic!("strict never reads the pre-state"))
            .unwrap_err();
        assert_eq!(refused.orphan_keys, vec![Key::name("old")]);
    }

    #[test]
    fn refusal_caps_the_listing() {
        let options = opts(IntegrityMode::Strict, IntegrityMode::Strict);
        let mut docs: Vec<(String, String)> = vec![("index".into(), "# Index\n".into())];
        for i in 0..25 {
            docs.push((format!("p{i:02}"), format!("# P{i}\n")));
        }
        let refs: Vec<(&str, &str)> = docs.iter().map(|(k, c)| (k.as_str(), c.as_str())).collect();
        let text = check_commit(&options, &graph(&refs), Debt::default)
            .unwrap_err()
            .to_string();
        assert!(text.contains("25 orphan(s)"), "{text}");
        assert!(text.contains("… and 5 more orphan(s)"), "{text}");
    }

    #[test]
    fn append_link_item_joins_a_trailing_list_or_starts_one() {
        let opts = FormatOptions::default();
        let parent = Key::name("topics/index");
        let child = Key::name("topics/sub/page");
        assert_eq!(
            append_link_item("# T\n\n- [A](a)\n", &parent, &child, "Page", &opts),
            "# T\n\n- [A](a)\n- [Page](sub/page)\n"
        );
        assert_eq!(
            append_link_item("# T\n\nText.\n", &parent, &child, "P [x]", &opts),
            "# T\n\nText.\n\n- [P \\[x\\]](sub/page)\n"
        );
    }
}
