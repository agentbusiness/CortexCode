//! Graph algorithms.

use std::collections::{HashMap, VecDeque};

/// Order the nodes of a directed acyclic graph so every edge points forward (Kahn's algorithm).
pub fn topological_order(edges: &[(u32, u32)]) -> Vec<u32> {
    let mut indegree: HashMap<u32, usize> = HashMap::new();
    let mut next: HashMap<u32, Vec<u32>> = HashMap::new();
    for (a, b) in edges {
        indegree.entry(*a).or_insert(0);
        *indegree.entry(*b).or_insert(0) += 1;
        next.entry(*a).or_default().push(*b);
    }
    let mut ready: VecDeque<u32> = indegree.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
    let mut out = Vec::new();
    while let Some(node) = ready.pop_front() {
        out.push(node);
        for n in next.get(&node).cloned().unwrap_or_default() {
            let d = indegree.entry(n).or_insert(0);
            *d -= 1;
            if *d == 0 {
                ready.push_back(n);
            }
        }
    }
    out
}

/// Fewest-hop route between two nodes using breadth-first expansion.
pub fn shortest_hops(edges: &[(u32, u32)], from: u32, to: u32) -> Option<usize> {
    let mut queue = VecDeque::from([(from, 0usize)]);
    let mut seen = std::collections::HashSet::from([from]);
    while let Some((node, depth)) = queue.pop_front() {
        if node == to {
            return Some(depth);
        }
        for (a, b) in edges {
            if *a == node && seen.insert(*b) {
                queue.push_back((*b, depth + 1));
            }
        }
    }
    None
}
