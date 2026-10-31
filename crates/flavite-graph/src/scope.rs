//! Exact induced-graph filtering. No IO, resolver, GUI, or layout restart.
use crate::{
    Direction, GraphBatch, GraphEdgeRecord, GraphScope, NodeId, NodeKind, NotePredicate,
    PropertyValue,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Current structural fields; saved query memberships are supplied by the API
/// from the very same database transaction as the graph base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteMetadata {
    pub tags: BTreeSet<String>,
    pub properties: BTreeMap<String, Vec<PropertyValue>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeResult {
    pub nodes: BTreeSet<NodeId>,
    pub edges: BTreeSet<GraphEdgeRecord>,
    pub total_base_notes: usize,
    pub matching_notes: usize,
    pub excluded_root: bool,
    pub missing_root: bool,
}

/// Evaluate predicates first, then bounded BFS over the induced eligible graph.
/// Unknown saved-query membership is an error, never an empty result.
pub fn evaluate(
    batch: &GraphBatch,
    metadata: &BTreeMap<NodeId, NoteMetadata>,
    saved_queries: &BTreeMap<String, BTreeSet<NodeId>>,
    scope: &GraphScope,
) -> Result<ScopeResult, &'static str> {
    scope.validate()?;
    let mut eligible = BTreeSet::new();
    for record in &batch.nodes {
        if record.id.kind == NodeKind::Unresolved {
            if scope.show_unresolved {
                eligible.insert(record.id);
            }
        } else if record.id.kind == NodeKind::Attachment {
            if scope.include_attachments {
                eligible.insert(record.id);
            }
        } else if matches_predicate(
            &scope.predicate,
            record.id,
            record.native_path.as_os_str().as_encoded_bytes(),
            metadata.get(&record.id),
            saved_queries,
        )? {
            eligible.insert(record.id);
        }
    }
    let eligible_edges: Vec<_> = batch
        .edges
        .iter()
        .filter(|edge| {
            (scope.include_embeds || edge.kind == crate::EdgeKind::Link)
                && eligible.contains(&edge.from)
                && eligible.contains(&edge.to)
        })
        .copied()
        .collect();
    let mut excluded_root = false;
    let mut missing_root = false;
    let nodes = if let Some(neighborhood) = &scope.neighborhood {
        let root = batch
            .nodes
            .iter()
            .find(|record| record.path_native == neighborhood.root_native)
            .map(|r| r.id);
        match root {
            None => {
                missing_root = true;
                BTreeSet::new()
            }
            Some(root) if !eligible.contains(&root) => {
                excluded_root = true;
                BTreeSet::new()
            }
            Some(root) => {
                let mut adjacency: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
                for edge in &eligible_edges {
                    if neighborhood.direction != Direction::In {
                        adjacency.entry(edge.from).or_default().push(edge.to);
                    }
                    if neighborhood.direction != Direction::Out {
                        adjacency.entry(edge.to).or_default().push(edge.from);
                    }
                }
                let mut visited = BTreeSet::from([root]);
                let mut queue = VecDeque::from([(root, 0)]);
                while let Some((node, depth)) = queue.pop_front() {
                    if depth == neighborhood.depth {
                        continue;
                    }
                    for next in adjacency.get(&node).into_iter().flatten() {
                        if visited.insert(*next) {
                            queue.push_back((*next, depth + 1));
                        }
                    }
                }
                visited
            }
        }
    } else {
        eligible
    };
    let edges = eligible_edges
        .into_iter()
        .filter(|e| nodes.contains(&e.from) && nodes.contains(&e.to))
        .collect();
    Ok(ScopeResult {
        matching_notes: nodes.iter().filter(|id| id.kind == NodeKind::Note).count(),
        total_base_notes: batch
            .nodes
            .iter()
            .filter(|r| r.id.kind == NodeKind::Note)
            .count(),
        nodes,
        edges,
        excluded_root,
        missing_root,
    })
}

fn matches_predicate(
    predicate: &NotePredicate,
    id: NodeId,
    path: &[u8],
    metadata: Option<&NoteMetadata>,
    saved_queries: &BTreeMap<String, BTreeSet<NodeId>>,
) -> Result<bool, &'static str> {
    Ok(match predicate {
        NotePredicate::All => true,
        NotePredicate::Tag { name } => metadata.is_some_and(|m| m.tags.contains(name)),
        NotePredicate::Property { key, value } => metadata.is_some_and(|m| {
            m.properties
                .get(key)
                .is_some_and(|values| values.iter().any(|v| property_equal(v, value)))
        }),
        NotePredicate::PathPrefix { native } => {
            path.starts_with(native)
                && (native.is_empty()
                    || native.last() == Some(&b'/')
                    || path.len() == native.len()
                    || path.get(native.len()) == Some(&b'/'))
        }
        NotePredicate::SavedQuery { query } => saved_queries
            .get(query)
            .ok_or("saved query membership unavailable")?
            .contains(&id),
        NotePredicate::And { clauses } => {
            let mut result = true;
            for clause in clauses {
                result &= matches_predicate(clause, id, path, metadata, saved_queries)?;
            }
            result
        }
        NotePredicate::Or { clauses } => {
            let mut result = false;
            for clause in clauses {
                result |= matches_predicate(clause, id, path, metadata, saved_queries)?;
            }
            result
        }
        NotePredicate::Not { clause } => {
            !matches_predicate(clause, id, path, metadata, saved_queries)?
        }
    })
}

fn property_equal(a: &PropertyValue, b: &PropertyValue) -> bool {
    match (a, b) {
        (PropertyValue::Number(a), PropertyValue::Number(b)) => {
            match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(a), Ok(b)) => a.is_finite() && b.is_finite() && a == b,
                _ => false,
            }
        }
        _ => a == b,
    }
}
