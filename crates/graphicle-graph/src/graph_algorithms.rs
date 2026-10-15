// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/analyze.py
//! Native, ordered unweighted Brandes and bounded directed-cycle algorithms.
//! These cover the NetworkX operations used by graphify/analyze.py.
use crate::{cluster::PythonRandom, graph::Graph};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

pub fn neighbors(graph: &Graph) -> Vec<Vec<usize>> {
    (0..graph.nodes.len())
        .map(|u| {
            let mut seen = HashSet::new();
            graph
                .out_edges(u)
                .into_iter()
                .filter_map(|(_, v, _)| seen.insert(v).then_some(v))
                .collect()
        })
        .collect()
}
pub fn centrality(
    graph: &Graph,
    sample: Option<usize>,
    edge_scores: bool,
) -> (Vec<f64>, Vec<(usize, usize, f64)>) {
    let n = graph.nodes.len();
    let adjacency = neighbors(graph);
    let mut edges = Vec::new();
    let mut lookup = BTreeMap::new();
    for (u, v, _) in graph.iter_edges() {
        if !lookup.contains_key(&(u, v)) {
            lookup.insert((u, v), edges.len());
            if !graph.directed {
                lookup.insert((v, u), edges.len());
            }
            edges.push((u, v, 0.0));
        }
    }
    let sources = match sample {
        Some(k) => PythonRandom::new(42).sample(n, k.min(n)),
        None => (0..n).collect(),
    };
    let mut result = vec![0.0; n];
    // Each source's pass is independent; the passes run in parallel and their
    // contributions are added in source order, so every floating-point sum
    // happens in the same order as one sequential loop.
    let workers = crate::par::workers(None).min(sources.len().max(1));
    let pass = |source: usize| {
        let mut stack = Vec::new();
        let mut predecessors = vec![Vec::new(); n];
        let mut sigma = vec![0.0; n];
        let mut distance = vec![usize::MAX; n];
        sigma[source] = 1.0;
        distance[source] = 0;
        let mut queue = VecDeque::from([source]);
        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let d = distance[v];
            let paths = sigma[v];
            for &w in &adjacency[v] {
                if distance[w] == usize::MAX {
                    queue.push_back(w);
                    distance[w] = d + 1;
                }
                if distance[w] == d + 1 {
                    sigma[w] += paths;
                    predecessors[w].push(v);
                }
            }
        }
        // (node, delta) in the order the sequential loop added them to
        // `result`, and (edge, contribution) in the order it added those.
        let mut delta = vec![0.0; n];
        let mut node_parts = Vec::with_capacity(stack.len());
        let mut edge_parts = Vec::new();
        while let Some(w) = stack.pop() {
            let coefficient = (1.0 + delta[w]) / sigma[w];
            for &v in &predecessors[w] {
                let c = sigma[v] * coefficient;
                if edge_scores {
                    edge_parts.push((lookup[&(v, w)], c));
                }
                delta[v] += c;
            }
            if w != source {
                node_parts.push((w, delta[w]));
            }
        }
        (node_parts, edge_parts)
    };
    for chunk in sources.chunks(workers.max(1) * 2) {
        for (node_parts, edge_parts) in crate::par::map(chunk, workers, |s| pass(*s)) {
            for (i, c) in edge_parts {
                edges[i].2 += c;
            }
            for (w, d) in node_parts {
                result[w] += d;
            }
        }
    }
    if n > 2 {
        let sampled: HashSet<_> = sources.iter().copied().collect();
        for (node, value) in result.iter_mut().enumerate() {
            let scale = if sample.is_none() {
                1.0 / ((n - 1) * (n - 2)) as f64
            } else if sampled.contains(&node) {
                1.0 / ((sources.len() - 1) * (n - 2)) as f64
            } else {
                1.0 / (sources.len() * (n - 2)) as f64
            };
            *value *= scale;
        }
    }
    if edge_scores && n > 1 {
        let scale = 1.0 / (sources.len() * (n - 1)) as f64;
        for (_, _, value) in &mut edges {
            *value *= scale;
        }
    }
    (result, edges)
}

/// `networkx.strongly_connected_components` (networkx 3.7, iterative Tarjan
/// with its whole-graph early exit) over the nodes of `order`, in that order,
/// with `successors` in dict order. Each component is returned as the
/// sequence its Python `set` was built in, which fixes its iteration order.
fn nx_scc(order: &[usize], successors: &dyn Fn(usize) -> Vec<usize>, n: usize) -> Vec<Vec<usize>> {
    let total = order.len();
    let mut found = Vec::new();
    if total == 0 {
        return found;
    }
    let mut lowlink: Vec<usize> = vec![0; n]; // 0: not yet preordered
    let mut in_scc = vec![false; n];
    let mut dfs: Vec<(usize, Vec<usize>, usize, bool)> = Vec::new(); // node, successors, next, lead
    let mut comp_stack: Vec<usize> = Vec::new();
    let mut index = 0usize;
    for &source in order {
        if lowlink[source] != 0 {
            continue;
        }
        index += 1;
        let root_low = index;
        lowlink[source] = index;
        dfs.push((source, successors(source), 0, true));
        while let Some(frame) = dfs.last_mut() {
            let v = frame.0;
            let mut descend = None;
            while frame.2 < frame.1.len() {
                let w = frame.1[frame.2];
                frame.2 += 1;
                if lowlink[w] == 0 {
                    descend = Some(w);
                    break;
                }
                // Back or cross arc.
                if !in_scc[w] && lowlink[v] > lowlink[w] {
                    frame.3 = false;
                    lowlink[v] = lowlink[w];
                    if lowlink[v] == root_low && index == total {
                        // `set(comp_stack)` then `update(dfs_v)`.
                        let mut members = comp_stack.clone();
                        members.extend(dfs.iter().map(|f| f.0));
                        found.push(members);
                        return found;
                    }
                }
            }
            if let Some(w) = descend {
                index += 1;
                lowlink[w] = index;
                dfs.push((w, successors(w), 0, true));
                continue;
            }
            let (v, _, _, lead) = dfs.pop().expect("a frame");
            if lead {
                let v_low = lowlink[v];
                let mut members = vec![v];
                while let Some(&top) = comp_stack.last() {
                    if lowlink[top] < v_low {
                        break;
                    }
                    members.push(comp_stack.pop().expect("a member"));
                }
                for &m in &members {
                    in_scc[m] = true;
                }
                found.push(members);
            } else {
                comp_stack.push(v);
                let parent = dfs.last_mut().expect("a parent frame");
                if lowlink[parent.0] > lowlink[v] {
                    parent.3 = false;
                    lowlink[parent.0] = lowlink[v];
                }
            }
        }
    }
    found
}

/// networkx's `_bounded_cycle_search` (Gupta and Suzumura): every simple
/// cycle through `start` of at most `bound` nodes, in the order networkx
/// yields them, stopping after `remaining`.
fn bounded(
    successors: &dyn Fn(usize) -> Vec<usize>,
    n: usize,
    start: usize,
    bound: usize,
    remaining: usize,
) -> Vec<Vec<usize>> {
    let mut cache: Vec<Option<std::rc::Rc<Vec<usize>>>> = vec![None; n];
    let mut neighbors = |v: usize| -> std::rc::Rc<Vec<usize>> {
        cache[v]
            .get_or_insert_with(|| std::rc::Rc::new(successors(v)))
            .clone()
    };
    // lock.get(w, bound); the start is locked at 0.
    let mut lock: Vec<usize> = vec![bound; n];
    lock[start] = 0;
    // B: which nodes to relax when a node's lock is raised. The order of a
    // set's members does not change the locks the relaxation reaches.
    let mut blocked: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut path = vec![start];
    let mut in_path = vec![false; n];
    in_path[start] = true;
    let mut stack = vec![(neighbors(start), 0usize)];
    let mut blen = vec![bound];
    let mut cycles = Vec::new();
    while let Some((nbrs, next)) = stack.last_mut() {
        let mut descend = None;
        while *next < nbrs.len() {
            let w = nbrs[*next];
            *next += 1;
            if w == start {
                cycles.push(path.clone());
                *blen.last_mut().expect("a length bound") = 1;
                if cycles.len() >= remaining {
                    return cycles;
                }
            } else if path.len() < lock[w] {
                descend = Some(w);
                break;
            }
        }
        if let Some(w) = descend {
            path.push(w);
            in_path[w] = true;
            blen.push(bound);
            lock[w] = path.len();
            let row = neighbors(w);
            stack.push((row, 0));
            continue;
        }
        stack.pop();
        let v = path.pop().expect("a path node");
        in_path[v] = false;
        let bl = blen.pop().expect("a length bound");
        if let Some(parent) = blen.last_mut() {
            *parent = (*parent).min(bl);
        }
        if bl < bound {
            let mut relax = vec![(bl, v)];
            while let Some((bl, u)) = relax.pop() {
                // `lock.get(u, bound) < bound - bl + 1`, in signed arithmetic.
                if (lock[u] as i64) < bound as i64 - bl as i64 + 1 {
                    lock[u] = bound + 1 - bl;
                    relax.extend(
                        blocked[u]
                            .iter()
                            .filter(|w| !in_path[**w])
                            .map(|w| (bl + 1, *w)),
                    );
                }
            }
        } else {
            for &w in neighbors(v).iter() {
                blocked[w].insert(v);
            }
        }
    }
    cycles
}

/// `networkx.simple_cycles(G, length_bound=bound)` for a DiGraph given as
/// successor lists in node order (networkx 3.7 on CPython 3.12), stopping
/// after `cap` cycles. networkx takes each search's pivot from a `set` of
/// nodes and, for a component smaller than half the remaining graph, walks
/// the subgraph view's nodes in that set's order. Upstream's CLI runs with
/// PYTHONHASHSEED=0 (#3641), so those orders are fixed; `hashes` holds each
/// node's `hash()` under that seed and the orders are replayed with
/// `python_compat::set_order`.
pub fn directed_cycles(
    adjacency: &[Vec<usize>],
    hashes: &[i64],
    bound: usize,
    cap: usize,
) -> Vec<Vec<usize>> {
    let mut cycles = Vec::new();
    if bound == 0 {
        return cycles;
    }
    for (u, row) in adjacency.iter().enumerate() {
        if row.contains(&u) {
            cycles.push(vec![u]);
            if cycles.len() >= cap {
                return cycles;
            }
        }
    }
    if bound == 1 {
        return cycles;
    }
    // `nx.DiGraph((u, v) for u, Gu in G.adj.items() for v in Gu if v != u)`:
    // nodes in their order of first appearance in that edge list.
    let n = adjacency.len();
    let mut order = Vec::new();
    let mut present = vec![false; n];
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (u, row) in adjacency.iter().enumerate() {
        for &v in row.iter().filter(|&&v| v != u) {
            for x in [u, v] {
                if !present[x] {
                    present[x] = true;
                    order.push(x);
                }
            }
            succ[u].push(v);
        }
    }
    let set_order = |members: &[usize]| {
        crate::python_compat::set_order(members.iter().map(|&m| (hashes[m], m)))
    };
    if bound == 2 {
        // `visited.intersection(Gu)`: Gu's members that were visited, in the
        // resulting set's order.
        let mut visited = vec![false; n];
        for &u in &order {
            let seen: Vec<usize> = succ[u].iter().copied().filter(|&v| visited[v]).collect();
            for v in set_order(&seen) {
                if succ[v].contains(&u) {
                    cycles.push(vec![v, u]);
                    if cycles.len() >= cap {
                        return cycles;
                    }
                }
            }
            visited[u] = true;
        }
        return cycles;
    }
    // _directed_cycle_search: removing a pivot from G shrinks every view.
    let removed = std::cell::RefCell::new(vec![false; n]);
    let mut live = order.len();
    let whole = |v: usize| -> Vec<usize> {
        let removed = removed.borrow();
        succ[v].iter().copied().filter(|&w| !removed[w]).collect()
    };
    let mut components: Vec<Vec<usize>> = nx_scc(&order, &whole, n)
        .into_iter()
        .filter(|c| c.len() >= 2)
        .collect();
    let mut member = vec![false; n];
    while let Some(component) = components.pop() {
        // `c` iterates in set order; `G.subgraph(c)` keeps `set(c)` (built
        // in that order) as the view's node filter.
        let c_order = set_order(&component);
        let view_order = set_order(&c_order);
        for &m in &component {
            member[m] = true;
        }
        let view = |v: usize| -> Vec<usize> {
            let removed = removed.borrow();
            succ[v]
                .iter()
                .copied()
                .filter(|&w| member[w] && !removed[w])
                .collect()
        };
        let pivot = c_order[0];
        cycles.extend(bounded(&view, n, pivot, bound, cap - cycles.len()));
        if cycles.len() >= cap {
            return cycles;
        }
        removed.borrow_mut()[pivot] = true;
        live -= 1;
        // The view's nodes: from its filter set when that set is under half
        // of G's nodes (FilterAtlas.__iter__), else in G's order.
        let nodes: Vec<usize> = {
            let removed = removed.borrow();
            if 2 * component.len() < live {
                view_order
                    .iter()
                    .copied()
                    .filter(|&m| !removed[m])
                    .collect()
            } else {
                order
                    .iter()
                    .copied()
                    .filter(|&m| member[m] && !removed[m])
                    .collect()
            }
        };
        let found = nx_scc(&nodes, &view, n);
        for &m in &component {
            member[m] = false;
        }
        components.extend(found.into_iter().filter(|c| c.len() >= 2));
    }
    cycles
}
