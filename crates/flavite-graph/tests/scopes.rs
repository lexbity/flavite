//! Exact scope membership and an independent breadth-by-depth reachability oracle.
use flavite_graph::scope::{NoteMetadata, evaluate};
use flavite_graph::{
    Direction, EdgeKind, GraphBatch, GraphEdgeRecord, GraphNodeRecord, GraphScope, Neighborhood,
    NodeId, NotePredicate,
};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
fn fixture() -> GraphBatch {
    GraphBatch {
        epoch: 7,
        generation: 12,
        nodes: (1..=5)
            .map(|id| {
                GraphNodeRecord::note(
                    id,
                    format!("notes/{id}.md"),
                    format!("notes/{id}.md").into_bytes(),
                    format!("{id}"),
                    None,
                    false,
                )
            })
            .collect(),
        edges: vec![(1, 2), (2, 3), (4, 2), (3, 3), (3, 5)]
            .into_iter()
            .map(|(a, b)| edge(a, b))
            .collect(),
    }
}
fn edge(from: u64, to: u64) -> GraphEdgeRecord {
    GraphEdgeRecord {
        from: NodeId::note(from),
        to: NodeId::note(to),
        kind: EdgeKind::Link,
    }
}
fn ids(values: &[u64]) -> BTreeSet<NodeId> {
    values.iter().map(|id| NodeId::note(*id)).collect()
}
fn scope(direction: Direction, depth: u8) -> GraphScope {
    GraphScope {
        neighborhood: Some(Neighborhood {
            root_native: b"notes/2.md".to_vec(),
            depth,
            direction,
        }),
        ..GraphScope::default()
    }
}
#[test]
fn directed_neighborhood_predicates_excluded_roots_and_path_component_boundaries() {
    let batch = fixture();
    let empty = BTreeMap::new();
    assert_eq!(
        evaluate(&batch, &empty, &BTreeMap::new(), &scope(Direction::In, 1))
            .unwrap()
            .nodes,
        ids(&[1, 2, 4])
    );
    assert_eq!(
        evaluate(&batch, &empty, &BTreeMap::new(), &scope(Direction::Out, 1))
            .unwrap()
            .nodes,
        ids(&[2, 3])
    );
    assert_eq!(
        evaluate(&batch, &empty, &BTreeMap::new(), &scope(Direction::Both, 2))
            .unwrap()
            .nodes,
        ids(&[1, 2, 3, 4, 5])
    );
    assert_eq!(
        evaluate(&batch, &empty, &BTreeMap::new(), &scope(Direction::Both, 0))
            .unwrap()
            .nodes,
        ids(&[2])
    );
    let mut metadata = BTreeMap::new();
    metadata.insert(
        NodeId::note(1),
        NoteMetadata {
            tags: BTreeSet::from(["keep".into()]),
            ..NoteMetadata::default()
        },
    );
    let mut filtered = scope(Direction::Both, 6);
    filtered.predicate = NotePredicate::Tag {
        name: "keep".into(),
    };
    let result = evaluate(&batch, &metadata, &BTreeMap::new(), &filtered).unwrap();
    assert!(
        result.excluded_root && result.nodes.is_empty(),
        "excluded root never bypasses predicate"
    );
    let filtered = GraphScope {
        predicate: NotePredicate::PathPrefix {
            native: b"note".to_vec(),
        },
        ..GraphScope::default()
    };
    assert!(
        evaluate(&batch, &metadata, &BTreeMap::new(), &filtered)
            .unwrap()
            .nodes
            .is_empty(),
        "prefix ends on a component boundary"
    );
    let filtered = GraphScope {
        predicate: NotePredicate::Or {
            clauses: vec![
                NotePredicate::Tag {
                    name: "keep".into(),
                },
                NotePredicate::Not {
                    clause: Box::new(NotePredicate::All),
                },
            ],
        },
        ..GraphScope::default()
    };
    assert_eq!(
        evaluate(&batch, &metadata, &BTreeMap::new(), &filtered)
            .unwrap()
            .nodes,
        ids(&[1])
    );
}
#[test]
fn default_hides_attachment_and_embed_layers_and_unknown_query_is_an_error() {
    let mut batch = fixture();
    batch.nodes.push(GraphNodeRecord::attachment(
        1,
        "photo.png",
        b"photo.png".to_vec(),
    ));
    batch.edges.push(GraphEdgeRecord {
        from: NodeId::note(1),
        to: NodeId::attachment(1),
        kind: EdgeKind::Embed,
    });
    let empty = BTreeMap::new();
    let result = evaluate(&batch, &empty, &BTreeMap::new(), &GraphScope::default()).unwrap();
    assert!(!result.nodes.contains(&NodeId::attachment(1)));
    let scope = GraphScope {
        include_attachments: true,
        include_embeds: true,
        ..GraphScope::default()
    };
    assert_eq!(
        evaluate(&batch, &empty, &BTreeMap::new(), &scope)
            .unwrap()
            .edges
            .len(),
        batch.edges.len()
    );
    let query = GraphScope {
        predicate: NotePredicate::SavedQuery { query: "x".into() },
        ..GraphScope::default()
    };
    assert!(evaluate(&batch, &empty, &BTreeMap::new(), &query).is_err());
}
proptest! {
 #![proptest_config(ProptestConfig::with_cases(1024))]
 #[test]
 fn bfs_matches_independent_repeated_edge_scan(pairs in prop::collection::vec((1u64..=8,1u64..=8),0..50),depth in 0u8..=6,direction in 0u8..3) {
    let mut batch=fixture();batch.nodes=(1..=8).map(|id|GraphNodeRecord::note(id,format!("notes/{id}.md"),format!("notes/{id}.md").into_bytes(),"note",None,false)).collect();batch.edges=pairs.iter().map(|&(a,b)|edge(a,b)).collect();
    let direction=match direction{0=>Direction::In,1=>Direction::Out,_=>Direction::Both};
    let mut expected=BTreeSet::from([2u64]);
    for _ in 0..depth {let previous=expected.clone();for &(a,b) in &pairs{if direction!=Direction::In && previous.contains(&a){expected.insert(b);}
      if direction!=Direction::Out && previous.contains(&b){expected.insert(a);}}}
    let result=evaluate(&batch,&BTreeMap::new(),&BTreeMap::new(),&scope(direction,depth)).unwrap();
    prop_assert_eq!(result.nodes,expected.into_iter().map(NodeId::note).collect());
 }
}
