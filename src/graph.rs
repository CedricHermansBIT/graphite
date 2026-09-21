//! Lightweight graph wrapper used by the UI and layout engine.
//!
//! We deliberately avoid petgraph for the hot rendering path because we need
//! very cheap adjacency queries on potentially millions of nodes/edges.

use ahash::{AHashMap, AHashSet};
use std::sync::Arc;

use crate::filter::{ComponentSort, ComponentSortOrder, ComponentTopology, FilterParams};
use crate::gfa::{GfaGraph, Strand};

/// Per-node display data (resolved once, cheap to clone for rendering).
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub seg_idx: usize,
    pub name: Arc<str>,
    pub length: usize,
    pub depth: Option<f64>,
    pub read_count: Option<u64>,
    /// Visual width in graph units (log-scaled from length).
    pub visual_len: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Link,
    Jump {
        distance: Option<i64>,
        shortcut: bool,
    },
}

/// Per-edge display data.
#[derive(Debug, Clone, Copy)]
pub struct EdgeInfo {
    pub from: usize, // node index
    pub from_strand: Strand,
    pub to: usize, // node index
    pub to_strand: Strand,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Circular,
    Linear,
    Branched,
}

impl ComponentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Circular => "Circular",
            Self::Linear => "Linear",
            Self::Branched => "Branched",
        }
    }
}

/// Cached component metadata for the browser and component-level navigation.
#[derive(Debug, Clone)]
pub struct ComponentSummary {
    pub nodes: Vec<usize>,
    pub kind: ComponentKind,
    pub total_length: usize,
    pub mean_depth: Option<f64>,
    pub total_read_count: Option<u64>,
}

/// The renderable graph, potentially a filtered subset of the full GFA.
pub struct ViewGraph {
    pub nodes: Vec<NodeInfo>,
    pub edges: Vec<EdgeInfo>,
    /// Map from segment index → node index (only present nodes).
    pub seg_to_node: AHashMap<usize, usize>,
    /// Connected components in display order.
    pub components: Vec<ComponentSummary>,
}

impl ViewGraph {
    pub fn from_gfa(gfa: &GfaGraph, filter: &FilterParams) -> Self {
        let candidate: Vec<bool> = gfa
            .segments
            .iter()
            .map(|segment| filter.accepts(segment))
            .collect();
        let (component_retained, component_ranks) = component_selection(gfa, filter, &candidate);

        // Apply segment predicates first, then component-level predicates to
        // the induced graph. This makes topology and component size describe
        // exactly what will be displayed.
        let mut segment_order: Vec<usize> = (0..gfa.segments.len())
            .filter(|&i| candidate[i] && component_retained[i])
            .collect();
        // Components are contiguous and ordered here. The layout keeps this
        // order when it packs them, so rank zero is displayed at the top-left.
        segment_order.sort_unstable_by_key(|&i| (component_ranks[i], i));
        let nodes: Vec<NodeInfo> = segment_order
            .into_iter()
            .map(|i| {
                let seg = &gfa.segments[i];
                let visual_len = visual_length(seg.length);
                NodeInfo {
                    seg_idx: i,
                    name: seg.name.clone(),
                    length: seg.length,
                    depth: seg.depth,
                    read_count: seg.read_count,
                    visual_len,
                }
            })
            .collect();

        let seg_to_node: AHashMap<usize, usize> = nodes
            .iter()
            .enumerate()
            .map(|(ni, node)| (node.seg_idx, ni))
            .collect();

        // --- Filter connections (only keep endpoints that are present) ---
        let mut edges: Vec<EdgeInfo> =
            Vec::with_capacity(gfa.links.len().saturating_add(gfa.jumps.len()));
        edges.extend(gfa.links.iter().filter_map(|link| {
            let from = *seg_to_node.get(&link.from)?;
            let to = *seg_to_node.get(&link.to)?;
            Some(EdgeInfo {
                from,
                from_strand: link.from_strand,
                to,
                to_strand: link.to_strand,
                kind: EdgeKind::Link,
            })
        }));
        edges.extend(gfa.jumps.iter().filter_map(|jump| {
            let from = *seg_to_node.get(&jump.from)?;
            let to = *seg_to_node.get(&jump.to)?;
            Some(EdgeInfo {
                from,
                from_strand: jump.from_strand,
                to,
                to_strand: jump.to_strand,
                kind: EdgeKind::Jump {
                    distance: jump.distance,
                    shortcut: jump.shortcut,
                },
            })
        }));

        let components = build_component_summaries(&nodes, &edges);

        Self {
            nodes,
            edges,
            seg_to_node,
            components,
        }
    }

    /// Adjacency list: node → list of (neighbour node idx, strand).
    pub fn build_adjacency(&self) -> Vec<Vec<(usize, Strand)>> {
        let n = self.nodes.len();
        let mut adj: Vec<Vec<(usize, Strand)>> = vec![Vec::new(); n];
        for e in &self.edges {
            adj[e.from].push((e.to, e.to_strand));
            adj[e.to].push((e.from, e.from_strand));
        }
        adj
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Connected component sizes (sorted descending).
    #[allow(dead_code)]
    pub fn component_sizes(&self) -> Vec<usize> {
        let n = self.nodes.len();
        let mut visited = vec![false; n];
        let adj = self.build_adjacency();
        let mut sizes = Vec::new();

        for start in 0..n {
            if visited[start] {
                continue;
            }
            let mut stack = vec![start];
            let mut size = 0;
            while let Some(v) = stack.pop() {
                if visited[v] {
                    continue;
                }
                visited[v] = true;
                size += 1;
                for &(nb, _) in &adj[v] {
                    if !visited[nb] {
                        stack.push(nb);
                    }
                }
            }
            sizes.push(size);
        }
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        sizes
    }
}

fn build_component_summaries(nodes: &[NodeInfo], edges: &[EdgeInfo]) -> Vec<ComponentSummary> {
    let n = nodes.len();
    let mut adjacency = vec![Vec::new(); n];
    let mut endpoint_degree = vec![0u8; n.saturating_mul(2)];
    let mut endpoint_links = AHashSet::default();
    for edge in edges {
        if edge.from >= n || edge.to >= n {
            continue;
        }
        adjacency[edge.from].push(edge.to);
        adjacency[edge.to].push(edge.from);
        let from = 2 * edge.from + usize::from(matches!(edge.from_strand, Strand::Forward));
        let to = 2 * edge.to + usize::from(matches!(edge.to_strand, Strand::Reverse));
        let unique = if from <= to { (from, to) } else { (to, from) };
        if endpoint_links.insert(unique) {
            endpoint_degree[from] = endpoint_degree[from].saturating_add(1);
            if from != to {
                endpoint_degree[to] = endpoint_degree[to].saturating_add(1);
            }
        }
    }

    let mut visited = vec![false; n];
    let mut components = Vec::new();
    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut component = Vec::new();
        while let Some(node) = stack.pop() {
            if visited[node] {
                continue;
            }
            visited[node] = true;
            component.push(node);
            for &neighbor in &adjacency[node] {
                if !visited[neighbor] {
                    stack.push(neighbor);
                }
            }
        }

        let mut total_length = 0usize;
        let mut depth_sum = 0.0;
        let mut depth_count = 0usize;
        let mut total_read_count = 0u64;
        let mut read_count_seen = false;
        let mut all_degree_one = true;
        let mut no_branches = true;
        let mut open_ends = 0usize;
        for &node in &component {
            let info = &nodes[node];
            total_length = total_length.saturating_add(info.length);
            if let Some(depth) = info.depth.filter(|depth| depth.is_finite()) {
                depth_sum += depth;
                depth_count += 1;
            }
            if let Some(read_count) = info.read_count {
                total_read_count = total_read_count.saturating_add(read_count);
                read_count_seen = true;
            }
            for degree in [endpoint_degree[2 * node], endpoint_degree[2 * node + 1]] {
                all_degree_one &= degree == 1;
                no_branches &= degree <= 1;
                open_ends += usize::from(degree == 0);
            }
        }
        let kind = if all_degree_one {
            ComponentKind::Circular
        } else if no_branches && open_ends == 2 {
            ComponentKind::Linear
        } else {
            ComponentKind::Branched
        };
        components.push(ComponentSummary {
            nodes: component,
            kind,
            total_length,
            mean_depth: (depth_count > 0).then(|| depth_sum / depth_count as f64),
            total_read_count: read_count_seen.then_some(total_read_count),
        });
    }
    components
}

fn component_selection(
    gfa: &GfaGraph,
    filter: &FilterParams,
    candidate: &[bool],
) -> (Vec<bool>, Vec<usize>) {
    let n = gfa.segments.len();
    let mut dsu = DisjointSet::new(n);
    for link in &gfa.links {
        if candidate[link.from] && candidate[link.to] {
            dsu.union(link.from, link.to);
        }
    }
    for jump in &gfa.jumps {
        if candidate[jump.from] && candidate[jump.to] {
            dsu.union(jump.from, jump.to);
        }
    }

    let mut sizes = vec![0usize; n];
    let mut total_lengths = vec![0u128; n];
    let mut depth_sums = vec![0.0f64; n];
    let mut depth_counts = vec![0usize; n];
    let mut total_read_counts = vec![0u128; n];
    for (segment, &present) in candidate.iter().enumerate() {
        if present {
            let root = dsu.find(segment);
            sizes[root] += 1;
            total_lengths[root] += gfa.segments[segment].length as u128;
            if let Some(depth) = gfa.segments[segment].depth.filter(|depth| depth.is_finite()) {
                depth_sums[root] += depth;
                depth_counts[root] += 1;
            }
            if let Some(read_count) = gfa.segments[segment].read_count {
                total_read_counts[root] += read_count as u128;
            }
        }
    }

    // Count unique links incident on each oriented segment endpoint. Reciprocal
    // duplicate GFA links must not turn a simple path or ring into a branch.
    let mut endpoint_degree = vec![0u8; n.saturating_mul(2)];
    let mut endpoint_links = AHashSet::default();
    for (from_segment, from_strand, to_segment, to_strand) in gfa
        .links
        .iter()
        .map(|link| (link.from, link.from_strand, link.to, link.to_strand))
        .chain(
            gfa.jumps
                .iter()
                .map(|jump| (jump.from, jump.from_strand, jump.to, jump.to_strand)),
        )
    {
        if !candidate[from_segment] || !candidate[to_segment] {
            continue;
        }
        let from = 2 * from_segment + usize::from(matches!(from_strand, Strand::Forward));
        let to = 2 * to_segment + usize::from(matches!(to_strand, Strand::Reverse));
        let edge = if from <= to { (from, to) } else { (to, from) };
        if endpoint_links.insert(edge) {
            endpoint_degree[from] = endpoint_degree[from].saturating_add(1);
            if from != to {
                endpoint_degree[to] = endpoint_degree[to].saturating_add(1);
            }
        }
    }

    let mut all_degree_one = vec![true; n];
    let mut no_branches = vec![true; n];
    let mut open_ends = vec![0usize; n];
    for (segment, &present) in candidate.iter().enumerate() {
        if !present {
            continue;
        }
        let root = dsu.find(segment);
        for degree in [
            endpoint_degree[2 * segment],
            endpoint_degree[2 * segment + 1],
        ] {
            all_degree_one[root] &= degree == 1;
            no_branches[root] &= degree <= 1;
            open_ends[root] += usize::from(degree == 0);
        }
    }

    let mut roots: Vec<usize> = sizes
        .iter()
        .enumerate()
        .filter(|&(root, &size)| {
            if size < filter.min_component_segments
                || filter
                    .max_component_segments
                    .is_some_and(|maximum| size > maximum)
            {
                return false;
            }
            match filter.component_topology {
                ComponentTopology::All => true,
                ComponentTopology::Circular => all_degree_one[root],
                ComponentTopology::Linear => no_branches[root] && open_ends[root] == 2,
                ComponentTopology::Branched => {
                    !all_degree_one[root] && !(no_branches[root] && open_ends[root] == 2)
                }
            }
        })
        .map(|(root, _)| root)
        .collect();
    roots.sort_unstable_by(|&a, &b| {
        let metric_order = match filter.component_sort {
            ComponentSort::SegmentCount => sizes[a].cmp(&sizes[b]),
            ComponentSort::TotalLength => total_lengths[a].cmp(&total_lengths[b]),
            ComponentSort::MeanDepth => {
                let a_depth = (depth_counts[a] > 0)
                    .then(|| depth_sums[a] / depth_counts[a] as f64);
                let b_depth = (depth_counts[b] > 0)
                    .then(|| depth_sums[b] / depth_counts[b] as f64);
                match (a_depth, b_depth) {
                    (Some(a), Some(b)) => a.total_cmp(&b),
                    (Some(_), None) => return std::cmp::Ordering::Less,
                    (None, Some(_)) => return std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            }
            ComponentSort::TotalReadCount => total_read_counts[a].cmp(&total_read_counts[b]),
        };
        let metric_order = match filter.component_sort_order {
            ComponentSortOrder::Descending => metric_order.reverse(),
            ComponentSortOrder::Ascending => metric_order,
        };
        metric_order.then_with(|| a.cmp(&b))
    });
    if filter.top_components > 0 {
        roots.truncate(filter.top_components);
    }

    let mut retained_roots = vec![false; n];
    let mut root_ranks = vec![usize::MAX; n];
    for (rank, root) in roots.into_iter().enumerate() {
        retained_roots[root] = true;
        root_ranks[root] = rank;
    }
    let retained: Vec<bool> = (0..n)
        .map(|segment| candidate[segment] && retained_roots[dsu.find(segment)])
        .collect();
    let component_ranks = (0..n)
        .map(|segment| root_ranks[dsu.find(segment)])
        .collect();
    (retained, component_ranks)
}

struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, mut item: usize) -> usize {
        let mut root = item;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        while self.parent[item] != item {
            let parent = self.parent[item];
            self.parent[item] = root;
            item = parent;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let mut a = self.find(a);
        let mut b = self.find(b);
        if a == b {
            return;
        }
        if self.rank[a] < self.rank[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        if self.rank[a] == self.rank[b] {
            self.rank[a] = self.rank[a].saturating_add(1);
        }
    }
}

/// Map sequence length → visual node length.
/// Linear so fragments are proportional to their actual lengths.
/// Capped at 2000 to prevent the force-directed layout from exploding.
fn visual_length(bp: usize) -> f32 {
    if bp == 0 {
        return 20.0;
    }
    // Linear scale: 1 kbp ≈ 100 visual units. No cap — long contigs look long.
    bp as f32 * 0.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{ComponentSort, ComponentSortOrder, ComponentTopology};
    use crate::gfa::{GfaVersion, Link, Segment};
    use memmap2::MmapMut;
    use std::ops::Range;

    fn test_gfa() -> GfaGraph {
        let lengths = [1000, 300, 300, 100, 100, 100, 50];
        let depths = [Some(100.0), Some(10.0), Some(10.0), Some(20.0), Some(20.0), Some(20.0), None];
        let read_counts = [Some(1), Some(100), Some(100), Some(10), Some(10), Some(10), None];
        let segments = (0..7)
            .map(|i| Segment {
                id: i,
                name: Arc::from(i.to_string()),
                seq_range: Range { start: 0, end: 0 },
                length: lengths[i],
                depth: depths[i],
                read_count: read_counts[i],
                tag_range: 0..0,
            })
            .collect();
        let link = |from, from_strand, to, to_strand| Link {
            from,
            from_strand,
            to,
            to_strand,
            overlap_range: Range { start: 0, end: 0 },
            tag_range: 0..0,
        };
        use Strand::{Forward as F, Reverse as R};
        GfaGraph {
            mmap: MmapMut::map_anon(1).unwrap().make_read_only().unwrap(),
            version: GfaVersion::Unspecified,
            headers: Vec::new(),
            segments,
            sequence_segment_count: 0,
            links: vec![
                link(0, F, 0, F), // circular singleton
                link(1, F, 2, F), // two-segment linear component
                link(3, F, 4, F), // three-segment branch
                link(3, F, 5, F),
                link(2, R, 1, R), // reciprocal duplicate of the path link
            ],
            jumps: Vec::new(),
            containments: Vec::new(),
            paths: Vec::new(),
            path_steps: Vec::new(),
            walks: Vec::new(),
            walk_steps: Vec::new(),
            tags: Vec::new(),
            name_index: Default::default(),
        }
    }

    fn names(graph: &ViewGraph) -> Vec<&str> {
        graph.nodes.iter().map(|node| node.name.as_ref()).collect()
    }

    #[test]
    fn component_filters_and_largest_first_order() {
        let gfa = test_gfa();

        let graph = ViewGraph::from_gfa(&gfa, &FilterParams::default());
        assert_eq!(&names(&graph)[..3], &["3", "4", "5"]);

        let mut filter = FilterParams::default();
        filter.component_topology = ComponentTopology::Circular;
        assert_eq!(names(&ViewGraph::from_gfa(&gfa, &filter)), ["0"]);

        filter.component_topology = ComponentTopology::Linear;
        filter.min_component_segments = 2;
        assert_eq!(names(&ViewGraph::from_gfa(&gfa, &filter)), ["1", "2"]);

        filter = FilterParams::default();
        filter.min_component_segments = 3;
        assert_eq!(names(&ViewGraph::from_gfa(&gfa, &filter)), ["3", "4", "5"]);

        filter = FilterParams::default();
        filter.top_components = 2;
        assert_eq!(
            names(&ViewGraph::from_gfa(&gfa, &filter)),
            ["3", "4", "5", "1", "2"]
        );
    }

    #[test]
    fn components_support_multiple_sort_metrics_and_directions() {
        let gfa = test_gfa();
        let mut filter = FilterParams::default();

        filter.component_sort = ComponentSort::TotalLength;
        assert_eq!(&names(&ViewGraph::from_gfa(&gfa, &filter))[..3], &["0", "1", "2"]);

        filter.component_sort = ComponentSort::MeanDepth;
        assert_eq!(&names(&ViewGraph::from_gfa(&gfa, &filter))[..4], &["0", "3", "4", "5"]);

        filter.component_sort = ComponentSort::TotalReadCount;
        assert_eq!(&names(&ViewGraph::from_gfa(&gfa, &filter))[..3], &["1", "2", "3"]);

        filter.component_sort = ComponentSort::SegmentCount;
        filter.component_sort_order = ComponentSortOrder::Ascending;
        assert_eq!(&names(&ViewGraph::from_gfa(&gfa, &filter))[..2], &["0", "6"]);
    }
}
