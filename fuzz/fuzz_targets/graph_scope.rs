//! Bounded scope decoding, deterministic predicates, and directed reachability.
#![no_main]
use flavite_graph::{Direction, EdgeKind, GraphBatch, GraphEdgeRecord, GraphNodeRecord,
    GraphScope, Neighborhood, NodeId};
use flavite_graph::scope::evaluate;
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};

fuzz_target!(|data: &[u8]| {
    if data.len() > 16384 { return; }
    if let Ok(scope) = serde_json::from_slice::<GraphScope>(data) {
        let batch = GraphBatch { epoch: 1, generation: 1, nodes: vec![], edges: vec![] };
        let first = evaluate(&batch, &BTreeMap::new(), &BTreeMap::new(), &scope);
        assert_eq!(first, evaluate(&batch, &BTreeMap::new(), &BTreeMap::new(), &scope));
        return;
    }
    if data.len() < 3 { return; }
    let count = 1+usize::from(data[0]%32);
    let root = u64::from(data[1])%count as u64;
    let depth = data[2]%7;
    let direction = match data[0]%3 { 0=>Direction::In, 1=>Direction::Out, _=>Direction::Both };
    let edges: BTreeSet<_> = data[3..].chunks_exact(2).take(256).map(|pair| GraphEdgeRecord {
        from: NodeId::note(u64::from(pair[0])%count as u64),
        to: NodeId::note(u64::from(pair[1])%count as u64), kind: EdgeKind::Link,
    }).collect();
    let batch = GraphBatch { epoch: 1, generation: 1, nodes: (0..count as u64).map(|id| {
        let path=format!("{id}.md");
        GraphNodeRecord::note(id,path.clone(),path.into_bytes(),id.to_string(),None,false)
    }).collect(), edges: edges.iter().cloned().collect() };
    let scope = GraphScope { neighborhood: Some(Neighborhood { root_native: format!("{root}.md").into_bytes(), depth, direction }), ..GraphScope::default() };
    let actual = evaluate(&batch, &BTreeMap::new(), &BTreeMap::new(), &scope).unwrap();
    let mut expected = BTreeSet::from([NodeId::note(root)]);
    for _ in 0..depth {
        let previous=expected.clone();
        for edge in &edges {
            if direction!=Direction::In && previous.contains(&edge.from) { expected.insert(edge.to); }
            if direction!=Direction::Out && previous.contains(&edge.to) { expected.insert(edge.from); }
        }
    }
    assert_eq!(actual.nodes,expected);
    assert!(actual.edges.iter().all(|e|expected.contains(&e.from)&&expected.contains(&e.to)));
});
