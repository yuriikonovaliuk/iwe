use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use crate::graph::{Graph, GraphContext};
use crate::model::Key;
use crate::query::block::BlockPredicate;
use crate::query::block_eval::BlockIndex;

/// A reference walk that only follows links found inside the blocks a
/// predicate selects — `$references: { via: "Is a" }` follows the genus chain
/// and nothing else. The predicate applies afresh at every hop, so a chain is
/// only as long as the documents that keep their links in that scope.
pub struct ViaWalk<'a> {
    graph: &'a Graph,
    via: &'a BlockPredicate,
    cache: Mutex<HashMap<Key, Vec<Key>>>,
}

impl<'a> ViaWalk<'a> {
    pub fn new(graph: &'a Graph, via: &'a BlockPredicate) -> Self {
        ViaWalk {
            graph,
            via,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The link targets of `key` that lie inside the scoped blocks.
    pub fn targets(&self, key: &Key) -> Vec<Key> {
        if let Some(targets) = self.cache.lock().expect("via cache").get(key) {
            return targets.clone();
        }
        let targets = if self.graph.maybe_key(key).is_some() {
            BlockIndex::build(self.graph, key).targets_within(self.via)
        } else {
            Vec::new()
        };
        self.cache
            .lock()
            .expect("via cache")
            .insert(key.clone(), targets.clone());
        targets
    }

    /// Documents reachable from `anchor` by following scoped links, with the
    /// distance at which each was first reached.
    pub fn outbound(&self, anchor: &Key, max_distance: u32) -> HashMap<Key, u32> {
        self.bfs(anchor, max_distance, |key| self.targets(key))
    }

    /// Documents that reach `anchor` by scoped links — the referrers whose
    /// scoped blocks link to it, transitively.
    pub fn inbound(&self, anchor: &Key, max_distance: u32) -> HashMap<Key, u32> {
        self.bfs(anchor, max_distance, |key| {
            let mut seen = HashSet::new();
            self.graph
                .get_reference_edges_to(key)
                .into_iter()
                .map(|node_id| self.graph.key_of(node_id))
                .filter(|referrer| seen.insert(referrer.clone()))
                .filter(|referrer| self.targets(referrer).contains(key))
                .collect()
        })
    }

    fn bfs(
        &self,
        anchor: &Key,
        max_distance: u32,
        neighbors: impl Fn(&Key) -> Vec<Key>,
    ) -> HashMap<Key, u32> {
        let mut out: HashMap<Key, u32> = HashMap::new();
        let mut visited: HashSet<Key> = HashSet::new();
        visited.insert(anchor.clone());
        let mut queue: VecDeque<(Key, u32)> = VecDeque::new();
        queue.push_back((anchor.clone(), 0));
        while let Some((current, distance)) = queue.pop_front() {
            if distance >= max_distance {
                continue;
            }
            let next_distance = distance + 1;
            for neighbor in neighbors(&current) {
                if !visited.insert(neighbor.clone()) {
                    continue;
                }
                if neighbor == *anchor {
                    continue;
                }
                out.entry(neighbor.clone()).or_insert(next_distance);
                queue.push_back((neighbor, next_distance));
            }
        }
        out
    }
}
