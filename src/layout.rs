use rayon::prelude::*;
use std::sync::{Arc, Mutex};

use crate::gfa::Strand;
use crate::graph::{EdgeKind, ViewGraph};
use crate::rust_layout;

pub type Pos2 = [f32; 2];

unsafe extern "C" {
    fn bandage_initial_layout(
        count: usize,
        edge_count: usize,
        from: *const u32,
        to: *const u32,
        length: *const f32,
        xy: *mut f32,
    ) -> std::ffi::c_int;
}

// ── Physics constants ─────────────────────────────────────────────────────────

/// Ideal spacing between physics nodes of the same segment AND between
/// unconnected nodes at equilibrium.  k = AREA_PER_NODE.sqrt().
const AREA_PER_NODE: f32 = 30_000.0;

/// Maximum physics nodes per GFA segment (caps very long contigs).
const MAX_PTS: usize = 64;

/// Desired length for graph-link springs expressed as a fraction of k.
/// Must be large enough that repulsion can overcome link attraction.
/// At 0.4*k, equilibrium sits comfortably between repulsion and spring.
const GRAPH_EDGE_RATIO: f32 = 0.4;

/// Stiffness for internal (within-segment) adjacent springs.
const SPRING_INTERNAL: f32 = 0.55;

/// Stiffness for bending springs (skip-one: i↔i+2). Resists chain folding.
/// Rest length = 2 × segment_spacing keeps the chain straight.
const SPRING_BEND: f32 = 0.45;

/// Stiffness for graph-link springs — deliberately weak so repulsion can compete.
const SPRING_LINK: f32 = 0.05;

/// During an active grab, linked contigs should follow the dragged segment
/// instead of remaining visually anchored to their previous positions.
const DRAG_LINK_SPRING_SCALE: f32 = 3.0;

/// Reduce near-field repulsion while dragging so attraction can pull connected
/// segments along without the component immediately pushing itself apart.
const DRAG_REPULSION_SCALE: f32 = 0.55;

/// Extra damping during a grab. This scales each non-pinned displacement before
/// the normal temperature clamp, reducing oscillation without making followers
/// feel stuck.
const DRAG_DAMPING_SCALE: f32 = 0.75;

/// If a node's drag force reverses direction between consecutive iterations,
/// damp that corrective step heavily. This suppresses branch-point chatter
/// without slowing followers that are still moving consistently toward the drag.
const DRAG_REVERSAL_DAMPING_SCALE: f32 = 0.10;

/// Unlike the normal relaxation pass, an active drag must not cool down over
/// time. Otherwise a long drag eventually leaves linked segments almost fixed
/// in place while the grabbed contig keeps moving away.
const DRAG_MOVE_LIMIT_SCALE: f32 = 0.85;

/// Bandage's "nearby pieces" drag uses an index-distance falloff with a default
/// strength of 100. Use the same curve for the dragged contig so it bends around
/// the grabbed point instead of translating as a rigid polyline.
const DRAG_FALLOFF_STRENGTH: f32 = 100.0;

// ── Layout ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBackend {
    Bandage,
    Rust,
}

impl LayoutBackend {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "bandage" | "ogdf" => Some(Self::Bandage),
            "rust" => Some(Self::Rust),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bandage => "bandage",
            Self::Rust => "rust",
        }
    }
}

#[derive(Clone)]
pub struct Layout {
    /// All physics-node positions, packed by segment.
    pub positions: Vec<Pos2>,

    /// For GFA node `ni`:
    ///   physics nodes at positions[node_pts_start[ni] .. node_pts_start[ni]+node_pts_count[ni]]
    pub node_pts_start: Vec<usize>,
    pub node_pts_count: Vec<usize>,

    /// Springs in SoA layout for cache-friendly iteration.
    /// (a, b, desired, stiffness) — using u32 indices to halve spring data size.
    springs_a: Vec<u32>,
    springs_b: Vec<u32>,
    springs_desired: Vec<f32>,
    springs_stiff: Vec<f32>,

    /// Maps every physics-node index → its GFA-node index.
    phys_to_node: Vec<u32>,

    /// Maps every physics-node index → its position within its chain (0 = first node).
    phys_chain_idx: Vec<u8>,

    /// Component id for each GFA node.
    comp_ids: Vec<usize>,
    components: Vec<Vec<usize>>,
    circular: Vec<bool>,
    fixed: Vec<bool>,
    active_points: Vec<usize>,
    grid: GridIndex,
    user_positioned: bool,
    drag_component: Option<usize>,

    /// Reusable displacement buffer — zeroed at the start of each step.
    disp: Vec<Pos2>,

    /// Previous raw force while dragging. Used only to detect direction
    /// reversals and suppress oscillation at highly connected branch points.
    prev_drag_force: Vec<Pos2>,

    revision: usize,
    pub iteration: usize,
    pub converged: bool,
}

impl Layout {
    // ── Accessors ─────────────────────────────────────────────────────────────

    #[inline]
    pub fn num_nodes(&self) -> usize {
        self.node_pts_start.len()
    }

    /// First physics-node position of segment `ni`.
    #[inline]
    pub fn start(&self, ni: usize) -> Pos2 {
        self.positions[self.node_pts_start[ni]]
    }

    /// Last physics-node position of segment `ni`.
    #[inline]
    pub fn end(&self, ni: usize) -> Pos2 {
        let s = self.node_pts_start[ni];
        let c = self.node_pts_count[ni];
        self.positions[s + c - 1]
    }

    /// Midpoint physics-node position of segment `ni`.
    #[inline]
    pub fn center(&self, ni: usize) -> Pos2 {
        let s = self.node_pts_start[ni];
        let c = self.node_pts_count[ni];
        self.positions[s + c / 2]
    }

    /// Outward endpoint for a given strand (Forward = end, Reverse = start).
    #[cfg(test)]
    #[inline]
    pub fn strand_endpoint(&self, ni: usize, strand: Strand) -> Pos2 {
        match strand {
            Strand::Forward => self.end(ni),
            Strand::Reverse => self.start(ni),
        }
    }

    /// Position at a fractional distance along the displayed segment polyline.
    ///
    /// This follows the current drawn geometry rather than assuming physics
    /// points stayed evenly spaced after layout/dragging.
    pub fn point_at_fraction(&self, ni: usize, fraction: f32) -> Pos2 {
        let points = self.pts(ni);
        if points.is_empty() {
            return [0.0, 0.0];
        }
        if points.len() == 1 {
            return points[0];
        }

        let fraction = fraction.clamp(0.0, 1.0);
        let mut total = 0.0_f32;
        for pair in points.windows(2) {
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            total += (dx * dx + dy * dy).sqrt();
        }
        if total <= f32::EPSILON {
            return points[0];
        }

        let target = fraction * total;
        let mut travelled = 0.0_f32;
        for pair in points.windows(2) {
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            let segment = (dx * dx + dy * dy).sqrt();
            if travelled + segment >= target && segment > f32::EPSILON {
                let t = ((target - travelled) / segment).clamp(0.0, 1.0);
                return [
                    pair[0][0] + dx * t,
                    pair[0][1] + dy * t,
                ];
            }
            travelled += segment;
        }
        *points.last().unwrap()
    }

    /// Slice of all physics-node positions belonging to segment `ni`.
    #[inline]
    pub fn pts(&self, ni: usize) -> &[Pos2] {
        let s = self.node_pts_start[ni];
        let c = self.node_pts_count[ni];
        &self.positions[s..s + c]
    }

    // ── Constructor ───────────────────────────────────────────────────────────

    #[cfg(test)]
    pub fn new_with_graph(graph: &ViewGraph) -> Self {
        Self::new_with_graph_backend(graph, LayoutBackend::Bandage)
    }

    pub fn new_with_graph_backend(graph: &ViewGraph, backend: LayoutBackend) -> Self {
        let n = graph.nodes.len();
        if n == 0 {
            return Self {
                positions: Vec::new(),
                node_pts_start: Vec::new(),
                node_pts_count: Vec::new(),
                springs_a: Vec::new(),
                springs_b: Vec::new(),
                springs_desired: Vec::new(),
                springs_stiff: Vec::new(),
                phys_to_node: Vec::new(),
                phys_chain_idx: Vec::new(),
                comp_ids: Vec::new(),
                components: Vec::new(),
                circular: Vec::new(),
                fixed: Vec::new(),
                active_points: Vec::new(),
                grid: GridIndex::default(),
                user_positioned: false,
                drag_component: None,
                disp: Vec::new(),
                prev_drag_force: Vec::new(),
                revision: 0,
                iteration: 0,
                converged: false,
            };
        }

        let k = AREA_PER_NODE.sqrt();

        // ── Connected components ──────────────────────────────────────────────
        let adj = graph.build_adjacency();
        let mut visited = vec![false; n];
        let mut components: Vec<Vec<usize>> = Vec::new();
        for i in 0..n {
            if visited[i] {
                continue;
            }
            let mut comp = Vec::new();
            let mut stack = vec![i];
            while let Some(v) = stack.pop() {
                if visited[v] {
                    continue;
                }
                visited[v] = true;
                comp.push(v);
                for &(nb, _) in &adj[v] {
                    if !visited[nb] {
                        stack.push(nb);
                    }
                }
            }
            components.push(comp);
        }
        // ViewGraph already groups components in the user-selected order.
        // The scan above discovers them in that same order; preserve it for
        // top-left to bottom-right packing.
        let mut comp_ids = vec![0usize; n];
        for (ci, comp) in components.iter().enumerate() {
            for &v in comp {
                comp_ids[v] = ci;
            }
        }

        // ── Physics-node count per segment ────────────────────────────────────
        // num_pts = max(2, floor(visual_len / k) + 1) clamped to [2, MAX_PTS]
        let mut node_pts_count = vec![2usize; n];
        for ni in 0..n {
            let vl = graph.nodes[ni].visual_len;
            node_pts_count[ni] = ((vl / k).floor() as usize + 1).clamp(2, MAX_PTS);
        }
        let total_pts: usize = node_pts_count.iter().sum();

        let mut node_pts_start = vec![0usize; n];
        let mut offset = 0usize;
        for ni in 0..n {
            node_pts_start[ni] = offset;
            offset += node_pts_count[ni];
        }
        debug_assert_eq!(offset, total_pts);

        // Endpoint adjacency preserves GFA orientation and recognises self-loops
        // and two-segment circles as well as ordinary rings.
        let mut ends = vec![Vec::new(); 2 * n];
        for e in &graph.edges {
            let a = 2 * e.from + usize::from(matches!(e.from_strand, Strand::Forward));
            let b = 2 * e.to + usize::from(matches!(e.to_strand, Strand::Reverse));
            ends[a].push(b);
            ends[b].push(a);
        }
        for neighbours in &mut ends {
            neighbours.sort_unstable();
            neighbours.dedup();
        }
        let circular: Vec<bool> = components
            .iter()
            .map(|comp| {
                comp.iter()
                    .all(|&v| ends[2 * v].len() == 1 && ends[2 * v + 1].len() == 1)
            })
            .collect();
        let fixed: Vec<bool> = components
            .iter()
            .enumerate()
            .map(|(ci, comp)| circular[ci] || (comp.len() == 1 && adj[comp[0]].is_empty()))
            .collect();
        // Even short circular contigs need enough samples to look round.
        for (ci, comp) in components.iter().enumerate() {
            if circular[ci] {
                for &v in comp {
                    node_pts_count[v] = node_pts_count[v].max((32 / comp.len()).clamp(2, 32));
                }
            }
        }
        let mut total_pts = 0;
        for v in 0..n {
            node_pts_start[v] = total_pts;
            total_pts += node_pts_count[v];
        }
        let mut positions = vec![[0.0; 2]; total_pts];
        // Allocate traversal state once, not once per component.
        let mut depth = vec![usize::MAX; n];
        let mut reverse = vec![false; n];
        for (ci, comp) in components.iter().enumerate() {
            if circular[ci] {
                let circumference: f32 = comp
                    .iter()
                    .map(|&v| graph.nodes[v].visual_len.max(k) + k * GRAPH_EDGE_RATIO)
                    .sum();
                let radius = circumference / std::f32::consts::TAU;
                let mut entry = comp[0] * 2;
                let mut distance = 0.0;
                for _ in 0..comp.len() {
                    let v = entry / 2;
                    let len = graph.nodes[v].visual_len.max(k);
                    let count = node_pts_count[v];
                    for j in 0..count {
                        let angle = (distance + len * j as f32 / (count - 1) as f32) / radius;
                        let index = if entry % 2 == 0 { j } else { count - 1 - j };
                        positions[node_pts_start[v] + index] =
                            [radius * angle.cos(), radius * angle.sin()];
                    }
                    distance += len + k * GRAPH_EDGE_RATIO;
                    entry = ends[entry ^ 1][0];
                }
            } else {
                // Start chains at a tip; use compact, length-aware BFS columns
                // for branching components.
                let root = *comp.iter().min_by_key(|&&v| adj[v].len()).unwrap();
                reverse[root] = ends[2 * root + 1].is_empty() && !ends[2 * root].is_empty();
                let mut queue = vec![root];
                depth[root] = 0;
                let mut head = 0;
                let mut widths: Vec<f32> = Vec::new();
                let mut counts: Vec<usize> = Vec::new();
                while head < queue.len() {
                    let v = queue[head];
                    head += 1;
                    let d = depth[v];
                    if widths.len() <= d {
                        widths.push(0.0);
                        counts.push(0);
                    }
                    widths[d] = widths[d].max(graph.nodes[v].visual_len);
                    counts[d] += 1;
                    for endpoint in 2 * v..2 * v + 2 {
                        for &entry in &ends[endpoint] {
                            let nb = entry / 2;
                            if depth[nb] == usize::MAX {
                                depth[nb] = d + 1;
                                reverse[nb] = entry % 2 == 1;
                                queue.push(nb);
                            }
                        }
                    }
                }
                let area: f32 = widths
                    .iter()
                    .zip(&counts)
                    .map(|(&w, &count)| (w + k) * k * 2.0 * count as f32)
                    .sum();
                let max_rows = (area.sqrt() / (k * 2.0)).ceil().max(1.0) as usize;
                let mut x = vec![0.0; widths.len()];
                for d in 1..x.len() {
                    x[d] = x[d - 1] + counts[d - 1].div_ceil(max_rows) as f32 * (widths[d - 1] + k);
                }
                let mut rows = vec![0; widths.len()];
                for v in queue {
                    let d = depth[v];
                    let column = rows[d] / max_rows;
                    let y = ((rows[d] % max_rows) as f32
                        - (counts[d].min(max_rows) - 1) as f32 * 0.5)
                        * k
                        * 2.0;
                    rows[d] += 1;
                    let count = node_pts_count[v];
                    for j in 0..count {
                        let index = if reverse[v] { count - 1 - j } else { j };
                        positions[node_pts_start[v] + index] = [
                            x[d] + column as f32 * (widths[d] + k)
                                + graph.nodes[v].visual_len * j as f32 / (count - 1) as f32,
                            y,
                        ];
                    }
                }
            }
        }

        // ── Build phys_to_node and phys_chain_idx lookups ────────────────────
        let mut phys_to_node: Vec<u32> = vec![0u32; total_pts];
        let mut phys_chain_idx: Vec<u8> = vec![0u8; total_pts];
        for ni in 0..n {
            let s = node_pts_start[ni];
            let c = node_pts_count[ni];
            for j in 0..c {
                phys_to_node[s + j] = ni as u32;
                phys_chain_idx[s + j] = j.min(255) as u8;
            }
        }

        let active_points: Vec<_> = (0..total_pts)
            .filter(|&pi| !fixed[comp_ids[phys_to_node[pi] as usize]])
            .collect();

        // ── Build spring list (SoA for cache efficiency) ──────────────────────
        let graph_edge_desired = k * GRAPH_EDGE_RATIO;
        let mut springs_a: Vec<u32> = Vec::new();
        let mut springs_b: Vec<u32> = Vec::new();
        let mut springs_desired: Vec<f32> = Vec::new();
        let mut springs_stiff: Vec<f32> = Vec::new();

        let mut push_spring = |a: usize, b: usize, desired: f32, stiff: f32| {
            springs_a.push(a as u32);
            springs_b.push(b as u32);
            springs_desired.push(desired);
            springs_stiff.push(stiff);
        };

        for ni in 0..n {
            if fixed[comp_ids[ni]] {
                continue;
            }
            let vl = graph.nodes[ni].visual_len;
            let npts = node_pts_count[ni];
            let seg_len = if npts > 1 { vl / (npts - 1) as f32 } else { vl };
            let ps = node_pts_start[ni];
            for j in 0..npts.saturating_sub(1) {
                push_spring(ps + j, ps + j + 1, seg_len, SPRING_INTERNAL);
            }
            for j in 0..npts.saturating_sub(2) {
                push_spring(ps + j, ps + j + 2, seg_len * 2.0, SPRING_BEND);
            }
        }

        for edge in &graph.edges {
            let u = edge.from;
            let v = edge.to;
            if u >= n || v >= n || fixed[comp_ids[u]] {
                continue;
            }
            let pu = match edge.from_strand {
                Strand::Forward => node_pts_start[u] + node_pts_count[u] - 1,
                Strand::Reverse => node_pts_start[u],
            };
            let pv = match edge.to_strand {
                Strand::Forward => node_pts_start[v],
                Strand::Reverse => node_pts_start[v] + node_pts_count[v] - 1,
            };
            let desired = match edge.kind {
                EdgeKind::Jump {
                    distance: Some(distance),
                    ..
                } if distance > 0 => graph_edge_desired + distance as f32 * 0.1,
                _ => graph_edge_desired,
            };
            push_spring(pu, pv, desired, SPRING_LINK);
        }

        let disp = vec![[0.0_f32; 2]; total_pts];
        let prev_drag_force = vec![[0.0_f32; 2]; total_pts];

        let mut layout = Self {
            positions,
            node_pts_start,
            node_pts_count,
            springs_a,
            springs_b,
            springs_desired,
            springs_stiff,
            phys_to_node,
            phys_chain_idx,
            comp_ids,
            components,
            circular,
            fixed,
            active_points,
            grid: GridIndex::default(),
            user_positioned: false,
            drag_component: None,
            disp,
            prev_drag_force,
            revision: 0,
            iteration: 0,
            converged: false,
        };
        layout.converged = match backend {
            LayoutBackend::Bandage => layout.seed_with_bandage(graph),
            LayoutBackend::Rust => layout.seed_with_rust(graph),
        };
        layout.pack_components();
        layout
    }

    /// Use Bandage's bundled OGDF FMMM implementation on connected, non-ring
    /// polylines. Isolated contigs and explicit circles need no force solve.
    fn seed_with_bandage(&mut self, graph: &ViewGraph) -> bool {
        if self.active_points.is_empty() {
            return true;
        }
        // Solve a reduced chain representation on large assemblies. Endpoints
        // and lengths are retained; render/interaction points are interpolated.
        let max_samples = if self.active_points.len() > 100_000 {
            2
        } else {
            MAX_PTS
        };
        let mut indices = vec![u32::MAX; self.positions.len()];
        let mut samples = Vec::new();
        let mut ranges = Vec::new();
        let mut from = Vec::new();
        let mut to = Vec::new();
        let mut lengths = Vec::new();
        for v in 0..self.num_nodes() {
            if self.fixed[self.comp_ids[v]] {
                continue;
            }
            let count = self.node_pts_count[v];
            let sample_count = count.min(max_samples);
            let start = samples.len();
            for j in 0..sample_count {
                let chain_index = j * (count - 1) / (sample_count - 1);
                let pi = self.node_pts_start[v] + chain_index;
                let index = samples.len();
                indices[pi] = index as u32;
                if j > 0 {
                    let previous = samples[index - 1];
                    from.push((index - 1) as u32);
                    to.push(index as u32);
                    lengths.push(
                        graph.nodes[v].visual_len * (pi - previous) as f32 / (count - 1) as f32,
                    );
                }
                samples.push(pi);
            }
            ranges.push(start..samples.len());
        }
        for i in 0..self.springs_a.len() {
            if self.springs_stiff[i] != SPRING_LINK {
                continue;
            }
            from.push(indices[self.springs_a[i] as usize]);
            to.push(indices[self.springs_b[i] as usize]);
            lengths.push(self.springs_desired[i]);
        }
        let mut output = vec![[0.0_f32; 2]; samples.len()];
        // SAFETY: buffers have the declared lengths, endpoint indices refer to
        // output, and the C++ boundary catches exceptions and retains no pointers.
        let status = unsafe {
            bandage_initial_layout(
                output.len(),
                from.len(),
                from.as_ptr(),
                to.as_ptr(),
                lengths.as_ptr(),
                output.as_mut_ptr().cast::<f32>(),
            )
        };
        if status != 0 || !output.iter().flatten().all(|v| v.is_finite()) {
            log::warn!("Bandage FMMM initialization failed ({status}); using fallback placement");
            return false;
        }
        for range in ranges {
            for i in range.start..range.end - 1 {
                let a = samples[i];
                let b = samples[i + 1];
                for pi in a..=b {
                    let t = (pi - a) as f32 / (b - a) as f32;
                    self.positions[pi] = [
                        output[i][0] * (1.0 - t) + output[i + 1][0] * t,
                        output[i][1] * (1.0 - t) + output[i + 1][1] * t,
                    ];
                }
            }
        }
        true
    }

    /// Use Graphite's Rust multilevel Barnes-Hut backend on the same reduced
    /// representation passed to Bandage/OGDF.
    fn seed_with_rust(&mut self, graph: &ViewGraph) -> bool {
        if self.active_points.is_empty() {
            return true;
        }

        let max_samples = if self.active_points.len() > 100_000 {
            2
        } else {
            MAX_PTS
        };
        let mut indices = vec![u32::MAX; self.positions.len()];
        let mut samples = Vec::new();
        let mut ranges = Vec::new();
        let mut from = Vec::new();
        let mut to = Vec::new();
        let mut lengths = Vec::new();

        for v in 0..self.num_nodes() {
            if self.fixed[self.comp_ids[v]] {
                continue;
            }
            let count = self.node_pts_count[v];
            let sample_count = count.min(max_samples);
            let start = samples.len();
            for j in 0..sample_count {
                let chain_index = j * (count - 1) / (sample_count - 1);
                let pi = self.node_pts_start[v] + chain_index;
                let index = samples.len();
                indices[pi] = index as u32;
                if j > 0 {
                    let previous = samples[index - 1];
                    from.push((index - 1) as u32);
                    to.push(index as u32);
                    lengths.push(
                        graph.nodes[v].visual_len * (pi - previous) as f32 / (count - 1) as f32,
                    );
                }
                samples.push(pi);
            }
            ranges.push(start..samples.len());
        }

        for i in 0..self.springs_a.len() {
            if self.springs_stiff[i] != SPRING_LINK {
                continue;
            }
            let a = indices[self.springs_a[i] as usize];
            let b = indices[self.springs_b[i] as usize];
            if a == u32::MAX || b == u32::MAX {
                log::warn!("Rust layout skipped an unresolved sampled spring endpoint");
                continue;
            }
            from.push(a);
            to.push(b);
            lengths.push(self.springs_desired[i]);
        }

        let initial: Vec<Pos2> = samples.iter().map(|&pi| self.positions[pi]).collect();
        let mut output = vec![[0.0_f32; 2]; samples.len()];
        if let Err(error) = rust_layout::initial_layout(
            output.len(),
            &from,
            &to,
            &lengths,
            &initial,
            &mut output,
        ) {
            log::warn!("Rust initial layout failed ({error}); using fallback placement");
            return false;
        }

        for range in ranges {
            if range.len() == 1 {
                self.positions[samples[range.start]] = output[range.start];
                continue;
            }
            for i in range.start..range.end - 1 {
                let a = samples[i];
                let b = samples[i + 1];
                for pi in a..=b {
                    let t = (pi - a) as f32 / (b - a) as f32;
                    self.positions[pi] = [
                        output[i][0] * (1.0 - t) + output[i + 1][0] * t,
                        output[i][1] * (1.0 - t) + output[i + 1][1] * t,
                    ];
                }
            }
        }
        true
    }

    /// Pack actual polyline bounds, including long singletons and circles.
    fn pack_components(&mut self) {
        let gap = AREA_PER_NODE.sqrt() * 3.0;
        let bounds: Vec<_> = self
            .components
            .iter()
            .map(|comp| {
                let mut lo = [f32::INFINITY; 2];
                let mut hi = [f32::NEG_INFINITY; 2];
                for &v in comp {
                    for p in self.pts(v) {
                        for d in 0..2 {
                            lo[d] = lo[d].min(p[d]);
                            hi[d] = hi[d].max(p[d]);
                        }
                    }
                }
                (lo, hi)
            })
            .collect();
        let target = bounds
            .iter()
            .map(|(lo, hi)| (hi[0] - lo[0] + gap) * (hi[1] - lo[1] + gap))
            .sum::<f32>()
            .sqrt()
            .max(gap);
        let (mut x, mut y, mut row_h) = (0.0_f32, 0.0_f32, 0.0_f32);
        for (ci, (lo, hi)) in bounds.into_iter().enumerate() {
            let w = hi[0] - lo[0];
            let h = hi[1] - lo[1];
            if x > 0.0 && x + w > target {
                x = 0.0;
                y += row_h + gap;
                row_h = 0.0;
            }
            for &v in &self.components[ci] {
                for p in &mut self.positions
                    [self.node_pts_start[v]..self.node_pts_start[v] + self.node_pts_count[v]]
                {
                    p[0] += x - lo[0];
                    p[1] += y - lo[1];
                }
            }
            x += w + gap;
            row_h = row_h.max(h);
        }
    }

    /// Move a whole contig (or a circular assembly) without stretching it.
    pub fn drag_to(&mut self, pos: Pos2, pi: usize) {
        if pi >= self.positions.len() {
            return;
        }
        self.user_positioned = true;
        let v = self.phys_to_node[pi] as usize;
        let ci = self.comp_ids[v];
        self.drag_component = Some(ci);
        let delta = [
            pos[0] - self.positions[pi][0],
            pos[1] - self.positions[pi][1],
        ];
        if self.circular[ci] {
            // Keep explicit circular assemblies intact when they are moved.
            for &node in &self.components[ci] {
                for p in &mut self.positions
                    [self.node_pts_start[node]..self.node_pts_start[node] + self.node_pts_count[node]]
                {
                    p[0] += delta[0];
                    p[1] += delta[1];
                }
            }
        } else {
            // Match Bandage's "nearby pieces" feel: the grabbed physics point
            // follows the cursor exactly, while progressively more distant
            // points on the same contig move by less. The force step below then
            // lets the polyline flex and settle naturally.
            let start = self.node_pts_start[v];
            let count = self.node_pts_count[v];
            let grabbed_chain_idx = pi - start;
            for j in 0..count {
                let index_distance = j.abs_diff(grabbed_chain_idx) as f32;
                let drag_strength =
                    2.0_f32.powf(-index_distance.powf(1.8) / DRAG_FALLOFF_STRENGTH);
                let p = &mut self.positions[start + j];
                p[0] += delta[0] * drag_strength;
                p[1] += delta[1] * drag_strength;
            }
        }
    }

    // ── Force-directed step ───────────────────────────────────────────────────

    pub fn step(
        &mut self,
        graph: &ViewGraph,
        params: &LayoutParams,
        attractor: Option<([f32; 2], usize)>,
    ) {
        self.revision += 1;
        let total_pts = self.positions.len();
        if total_pts == 0 {
            self.converged = true;
            self.iteration += 1;
            return;
        }
        let _ = graph;
        let attractor = attractor.filter(|(_, pi)| *pi < total_pts);
        if let Some((pos, pi)) = attractor {
            self.drag_to(pos, pi);
        }

        if self.active_points.is_empty() || self.drag_component.is_some_and(|ci| self.fixed[ci]) {
            self.iteration += 1;
            self.converged = attractor.is_none();
            return;
        }
        let k = AREA_PER_NODE.sqrt();
        // Normal relaxation cools over time to settle the graph. During an
        // active grab, keep a larger non-decaying movement allowance so linked
        // segments can continue following even after a long or very large drag.
        let t0 = k * 0.25;
        let progress =
            (self.iteration as f32 / params.max_iter.max(1) as f32).clamp(0.0, 1.0);
        let temp = if attractor.is_some() {
            k * DRAG_MOVE_LIMIT_SCALE
        } else {
            t0 * (-5.0 * progress).exp().max(0.01)
        };

        let k2 = k * k;
        // cell_size = query_r so each query only checks 3×3 = 9 cells (was 15×15=225).
        let query_r = k * 3.0;
        let cell_size = query_r;
        #[cfg(test)]
        let profile_start = std::time::Instant::now();
        self.grid.rebuild(
            &self.positions,
            cell_size,
            &self.active_points,
            &self.phys_to_node,
            &self.comp_ids,
            self.drag_component,
        );
        let grid = &self.grid;

        #[cfg(test)]
        let grid_time = profile_start.elapsed();
        // ── Repulsion (parallel) ─────────────────────────────────────────────
        // Zero the reusable displacement buffer.
        for d in &mut self.disp {
            *d = [0.0, 0.0];
        }

        // Split borrows so the parallel closure can access immutable fields
        // while writing into `disp`.
        let positions = &self.positions;
        let phys_to_node = &self.phys_to_node;
        let phys_chain = &self.phys_chain_idx;

        self.disp
            .par_iter_mut()
            .enumerate()
            .with_min_len(512)
            .for_each(|(pi, dpv)| {
                let ni = phys_to_node[pi] as usize;
                if self.fixed[self.comp_ids[ni]]
                    || self
                        .drag_component
                        .is_some_and(|ci| ci != self.comp_ids[ni])
                {
                    return;
                }
                let ji = phys_chain[pi];
                let pv = positions[pi];
                grid.query_nearby(&pv, self.comp_ids[ni], |pj| {
                    if pj == pi {
                        return;
                    }
                    let nj = phys_to_node[pj] as usize;
                    if self.comp_ids[ni] != self.comp_ids[nj] {
                        return;
                    }
                    if nj == ni {
                        // Skip pairs covered by adjacent/bending springs (chain dist < 3).
                        if phys_chain[pj].abs_diff(ji) < 3 {
                            return;
                        }
                    }
                    let dx = pv[0] - positions[pj][0];
                    let dy = pv[1] - positions[pj][1];
                    // FR repulsion: force = k²/dist in direction (dx,dy).
                    // Equivalent (no sqrt): fx = k²·dx/dist², fy = k²·dy/dist²
                    let dist2 = (dx * dx + dy * dy).max(1.0);
                    if dist2 > query_r * query_r {
                        return;
                    }
                    let repulsion_scale = if attractor.is_some() {
                        DRAG_REPULSION_SCALE
                    } else {
                        1.0
                    };
                    dpv[0] += repulsion_scale * k2 * dx / dist2;
                    dpv[1] += repulsion_scale * k2 * dy / dist2;
                });
            });

        #[cfg(test)]
        let repulsion_time = profile_start.elapsed();
        // ── Hookean springs (cache-friendly SoA, sequential) ─────────────────
        let disp = &mut self.disp;
        for i in 0..self.springs_a.len() {
            let pi = self.springs_a[i] as usize;
            let pj = self.springs_b[i] as usize;
            if self
                .drag_component
                .is_some_and(|ci| ci != self.comp_ids[self.phys_to_node[pi] as usize])
            {
                continue;
            }
            let desired = self.springs_desired[i];
            let mut stiff = self.springs_stiff[i];
            if attractor.is_some() && stiff == SPRING_LINK {
                stiff *= DRAG_LINK_SPRING_SCALE;
            }
            let dx = self.positions[pj][0] - self.positions[pi][0];
            let dy = self.positions[pj][1] - self.positions[pi][1];
            let dist = (dx * dx + dy * dy).sqrt().max(0.001);
            let f = (dist - desired) * stiff;
            let fx = dx / dist * f;
            let fy = dy / dist * f;
            disp[pi][0] += fx;
            disp[pi][1] += fy;
            disp[pj][0] -= fx;
            disp[pj][1] -= fy;
        }

        #[cfg(test)]
        let spring_time = profile_start.elapsed();
        // ── Pinned-node drag ─────────────────────────────────────────────────
        let grabbed_pi = if let Some((pos, pi)) = attractor {
            self.disp[pi] = [0.0, 0.0];
            Some((pos, pi))
        } else {
            None
        };

        // ── Clamp and apply ───────────────────────────────────────────────────
        let mut max_move = 0.0_f32;
        for &pi in &self.active_points {
            if self
                .drag_component
                .is_some_and(|ci| ci != self.comp_ids[self.phys_to_node[pi] as usize])
            {
                continue;
            }
            if attractor.is_some_and(|(_, grabbed)| pi == grabbed) {
                continue;
            }
            let raw_dx = self.disp[pi][0];
            let raw_dy = self.disp[pi][1];
            let damping = if attractor.is_some() {
                let previous = self.prev_drag_force[pi];
                let reversing = raw_dx * previous[0] + raw_dy * previous[1] < 0.0;
                self.prev_drag_force[pi] = [raw_dx, raw_dy];
                if reversing {
                    DRAG_DAMPING_SCALE * DRAG_REVERSAL_DAMPING_SCALE
                } else {
                    DRAG_DAMPING_SCALE
                }
            } else {
                self.prev_drag_force[pi] = [0.0, 0.0];
                1.0
            };
            let dx = raw_dx * damping;
            let dy = raw_dy * damping;
            let d = (dx * dx + dy * dy).sqrt().max(0.001);
            let clamped = d.min(temp);
            self.positions[pi][0] += dx / d * clamped;
            self.positions[pi][1] += dy / d * clamped;
            if clamped > max_move {
                max_move = clamped;
            }
        }
        if let Some((pos, pi)) = grabbed_pi {
            self.prev_drag_force[pi] = [0.0, 0.0];
            self.positions[pi] = pos;
        }

        #[cfg(test)]
        if self.iteration == 1 && std::env::var_os("GFA_PROFILE").is_some() {
            eprintln!(
                "grid {:?}, repulsion {:?}, springs {:?}, apply {:?}",
                grid_time,
                repulsion_time - grid_time,
                spring_time - repulsion_time,
                profile_start.elapsed() - spring_time
            );
        }
        self.iteration += 1;
        if attractor.is_some() {
            self.converged = false;
            return;
        }
        let min_iter = (params.max_iter / 4).max(50);
        self.converged = (self.iteration >= min_iter && max_move < k * 0.005)
            || self.iteration >= params.max_iter;
        if !self.user_positioned && (self.iteration % 20 == 0 || self.converged) {
            self.pack_components();
        }
    }
}

// ── LayoutParams ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct LayoutParams {
    pub max_iter: usize,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self { max_iter: 600 }
    }
}

// ── GridIndex ────────────────────────────────────────────────────────────────
// Hashed cells: expected linear build and constant-time cell lookup.
// Dense cells still require pairwise near-field interactions.

#[derive(Clone, Default)]
struct GridIndex {
    heads: ahash::AHashMap<(usize, i32, i32), usize>,
    // Flat linked buckets avoid a heap allocation for every occupied cell.
    entries: Vec<(usize, usize)>,
    cell_size: f32,
}

impl GridIndex {
    fn rebuild(
        &mut self,
        positions: &[Pos2],
        cell_size: f32,
        active: &[usize],
        owners: &[u32],
        components: &[usize],
        component_filter: Option<usize>,
    ) {
        self.heads.clear();
        self.entries.clear();
        self.cell_size = cell_size;
        for &pi in active {
            if component_filter.is_some_and(|ci| ci != components[owners[pi] as usize]) {
                continue;
            }
            let p = positions[pi];
            let key = (
                components[owners[pi] as usize],
                (p[0] / cell_size).floor() as i32,
                (p[1] / cell_size).floor() as i32,
            );
            let prev = self
                .heads
                .insert(key, self.entries.len())
                .unwrap_or(usize::MAX);
            self.entries.push((pi, prev));
        }
    }
    fn query_nearby(&self, p: &Pos2, component: usize, mut f: impl FnMut(usize)) {
        let cx = (p[0] / self.cell_size).floor() as i32;
        let cy = (p[1] / self.cell_size).floor() as i32;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let mut current = self
                    .heads
                    .get(&(component, cx + dx, cy + dy))
                    .copied()
                    .unwrap_or(usize::MAX);
                while current != usize::MAX {
                    let (pi, next) = self.entries[current];
                    f(pi);
                    current = next;
                }
            }
        }
    }
}

// ── LayoutRunner ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct DragState {
    held: Option<(Pos2, usize)>,
    pending: Option<(Pos2, usize)>,
}

pub struct LayoutRunner {
    pub layout: Arc<Mutex<Layout>>,
    pub running: Arc<std::sync::atomic::AtomicBool>,
    /// Grabbed physics node: (world_pos, physics_node_index).
    attractor: Arc<Mutex<DragState>>,
}

impl LayoutRunner {
    pub fn start_with_backend(
        graph: Arc<ViewGraph>,
        params: LayoutParams,
        backend: LayoutBackend,
        publish_interval: std::time::Duration,
    ) -> Self {
        let mut local = Layout::new_with_graph_backend(&graph, backend);
        let layout = Arc::new(Mutex::new(local.clone()));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let attractor = Arc::new(Mutex::new(DragState::default()));

        let layout2 = layout.clone();
        let running2 = running.clone();
        let attractor2 = attractor.clone();
        std::thread::spawn(move || {
            let mut published = std::time::Instant::now();
            let mut was_dragging = false;
            loop {
                if !running2.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // Retain the final cursor update even if release arrives before
                // the worker finishes its previous iteration.
                let att = attractor2.lock().ok().and_then(|mut g| {
                    let pending = g.pending.take();
                    g.held.or(pending)
                });
                if was_dragging != att.is_some() {
                    local.iteration = 0;
                    local.converged = false;
                }
                was_dragging = att.is_some();
                if local.converged && att.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(8));
                    continue;
                }
                if local.converged && att.is_some() {
                    local.converged = false;
                }
                local.step(&graph, &params, att);
                if published.elapsed() >= publish_interval || local.converged {
                    if let Ok(mut shared) = layout2.lock() {
                        shared.positions.clone_from(&local.positions);
                        shared.revision = local.revision;
                        shared.iteration = local.iteration;
                        shared.converged = local.converged;
                    }
                    published = std::time::Instant::now();
                }
                if att.is_some() {
                    std::thread::sleep(std::time::Duration::from_millis(4));
                }
            }
            if let Ok(mut shared) = layout2.lock() {
                *shared = local;
            }
            running2.store(false, std::sync::atomic::Ordering::Relaxed);
        });

        Self {
            layout,
            running,
            attractor,
        }
    }

    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Option<Layout> {
        self.layout.lock().ok().map(|g| g.clone())
    }

    pub fn update_snapshot(&self, target: &mut Layout) -> bool {
        if let Ok(shared) = self.layout.try_lock() {
            if target.revision == shared.revision {
                return false;
            }
            target.positions.clone_from(&shared.positions);
            target.revision = shared.revision;
            target.iteration = shared.iteration;
            target.converged = shared.converged;
            return true;
        }
        false
    }

    /// Set the grabbed physics node and cursor world position, or None to release.
    pub fn set_attractor(&self, val: Option<([f32; 2], usize)>) {
        if let Ok(mut a) = self.attractor.lock() {
            a.held = val;
            if val.is_some() {
                a.pending = val;
            }
        }
    }
}
impl Drop for LayoutRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeInfo, NodeInfo};

    fn graph(lengths: &[f32], links: &[(usize, Strand, usize, Strand)]) -> ViewGraph {
        ViewGraph {
            nodes: lengths
                .iter()
                .enumerate()
                .map(|(i, &len)| NodeInfo {
                    seg_idx: i,
                    name: Arc::from(i.to_string()),
                    length: (len * 10.0) as usize,
                    depth: None,
                    read_count: None,
                    visual_len: len,
                })
                .collect(),
            edges: links
                .iter()
                .map(|&(from, from_strand, to, to_strand)| EdgeInfo {
                    from,
                    from_strand,
                    to,
                    to_strand,
                    kind: EdgeKind::Link,
                })
                .collect(),
            seg_to_node: Default::default(),
            components: Vec::new(),
        }
    }

    fn assert_separated(layout: &Layout) {
        let bounds: Vec<_> = layout
            .components
            .iter()
            .map(|comp| {
                let mut lo = [f32::INFINITY; 2];
                let mut hi = [f32::NEG_INFINITY; 2];
                for &v in comp {
                    for p in layout.pts(v) {
                        for d in 0..2 {
                            lo[d] = lo[d].min(p[d]);
                            hi[d] = hi[d].max(p[d]);
                        }
                    }
                }
                (lo, hi)
            })
            .collect();
        for i in 0..bounds.len() {
            for j in 0..i {
                let (a, b) = bounds[i];
                let (c, d) = bounds[j];
                assert!(b[0] < c[0] || d[0] < a[0] || b[1] < c[1] || d[1] < a[1]);
            }
        }
    }

    #[test]
    fn packing_preserves_view_graph_component_order() {
        use Strand::Forward as F;
        let graph = graph(&[100.0, 100.0, 100.0], &[(1, F, 2, F)]);
        let layout = Layout::new_with_graph(&graph);

        assert_eq!(layout.components[0], vec![0]);
        assert_eq!(layout.components[1].len(), 2);
        assert!(layout.components[1].contains(&1));
        assert!(layout.components[1].contains(&2));
    }

    #[test]
    fn oriented_rings_and_long_singletons_remain_separate() {
        use Strand::{Forward as F, Reverse as R};
        let graph = graph(
            &[800.0, 1200.0, 2000.0, 3000.0, 100000.0, 50000.0],
            &[(0, F, 1, R), (1, R, 2, F), (2, F, 0, F), (3, F, 3, F)],
        );
        let mut layout = Layout::new_with_graph(&graph);
        assert_eq!(layout.circular.iter().filter(|&&v| v).count(), 2);
        assert_separated(&layout);
        for e in &graph.edges {
            let a = layout.strand_endpoint(e.from, e.from_strand);
            let b = match e.to_strand {
                F => layout.start(e.to),
                R => layout.end(e.to),
            };
            assert!((a[0] - b[0]).hypot(a[1] - b[1]) < 71.0);
        }
        let before = layout.pts(3).to_vec();
        for _ in 0..60 {
            layout.step(&graph, &LayoutParams { max_iter: 60 }, None);
        }
        for (a, b) in before.windows(2).zip(layout.pts(3).windows(2)) {
            assert!(
                ((a[0][0] - a[1][0]).hypot(a[0][1] - a[1][1])
                    - (b[0][0] - b[1][0]).hypot(b[0][1] - b[1][1]))
                .abs()
                    < 0.02
            );
        }
        assert_separated(&layout);
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn two_node_cycle_and_duplicate_reverse_links() {
        use Strand::{Forward as F, Reverse as R};
        let graph = graph(&[600.0, 600.0], &[(0, F, 1, F), (1, F, 0, F), (1, R, 0, R)]);
        let layout = Layout::new_with_graph(&graph);
        assert!(layout.circular[0]);
    }

    #[test]
    fn dragging_pins_target_and_flexes_contig() {
        let graph = graph(&[4000.0], &[]);
        let mut layout = Layout::new_with_graph(&graph);
        let before = layout.pts(0).to_vec();
        let start = layout.node_pts_start[0];
        let pi = start + layout.node_pts_count[0] / 2;
        let target = [layout.positions[pi][0] + 500.0, layout.positions[pi][1] + 250.0];
        layout.step(&graph, &LayoutParams::default(), Some((target, pi)));

        assert_eq!(layout.positions[pi], target);

        let grabbed_move = (layout.positions[pi][0] - before[pi - start][0])
            .hypot(layout.positions[pi][1] - before[pi - start][1]);
        let end_move = (layout.positions[start][0] - before[0][0])
            .hypot(layout.positions[start][1] - before[0][1]);

        assert!(end_move > 0.0, "the rest of the contig should follow the grab");
        assert!(
            end_move < grabbed_move,
            "drag falloff should let the grabbed contig bend instead of translating rigidly"
        );
    }

    #[test]
    fn drag_force_reversal_is_damped() {
        let graph = graph(&[1200.0, 1200.0], &[]);
        let mut layout = Layout::new_with_graph(&graph);
        let pi = layout.node_pts_start[0];
        layout.prev_drag_force[pi] = [100.0, 0.0];
        layout.disp[pi] = [-100.0, 0.0];

        let raw_dx = layout.disp[pi][0];
        let raw_dy = layout.disp[pi][1];
        let previous = layout.prev_drag_force[pi];
        let reversing = raw_dx * previous[0] + raw_dy * previous[1] < 0.0;
        assert!(reversing);
        let damped = raw_dx * DRAG_DAMPING_SCALE * DRAG_REVERSAL_DAMPING_SCALE;
        assert!(damped.abs() < raw_dx.abs() * 0.2);
    }

    #[test]
    fn linked_segments_keep_following_during_long_far_drag() {
        use Strand::Forward as F;
        let graph = graph(&[1200.0, 1200.0, 1200.0], &[(0, F, 1, F), (1, F, 2, F)]);
        let mut layout = Layout::new_with_graph(&graph);

        let grabbed = layout.node_pts_start[0] + layout.node_pts_count[0] - 1;
        let follower_before = layout.center(2);
        let target = [
            layout.positions[grabbed][0] + 20_000.0,
            layout.positions[grabbed][1] + 4_000.0,
        ];

        // Keep holding the same far-away target long enough that the normal
        // relaxation temperature would have cooled almost completely.
        for _ in 0..200 {
            layout.step(&graph, &LayoutParams::default(), Some((target, grabbed)));
        }

        let follower_after = layout.center(2);
        let follower_move = (follower_after[0] - follower_before[0])
            .hypot(follower_after[1] - follower_before[1]);

        assert_eq!(layout.positions[grabbed], target);
        assert!(
            follower_move > 5_000.0,
            "linked segments should continue following a long drag; moved only {follower_move}"
        );
    }

    /// Opt-in benchmark against a local assembly, without opening a GUI.
    #[test]
    #[ignore = "set GFA_BENCH_PATH to a local GFA file"]
    fn benchmark_gfa() {
        let path = std::env::var("GFA_BENCH_PATH").expect("GFA_BENCH_PATH is required");
        let start = std::time::Instant::now();
        let parsed = crate::gfa::parse_gfa(&path).unwrap();
        let parse_time = start.elapsed();
        let graph = ViewGraph::from_gfa(&parsed, &crate::filter::FilterParams::default());
        let start = std::time::Instant::now();
        let mut layout = Layout::new_with_graph(&graph);
        let init = start.elapsed();
        let steps = std::env::var("GFA_BENCH_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let start = std::time::Instant::now();
        for _ in 0..steps {
            layout.step(&graph, &LayoutParams::default(), None);
        }
        eprintln!("{}: {} segments, {} links, {} components, {} rings, {} physics points; parse {:?}, initial layout {:?}, {} steps {:?}",
            path, graph.nodes.len(), graph.edges.len(), layout.components.len(),
            layout.circular.iter().filter(|&&v| v).count(), layout.positions.len(), parse_time, init, steps, start.elapsed());
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
        if let Ok(output) = std::env::var("GFA_BENCH_OUTPUT") {
            let file = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
            serde_json::to_writer(
                file,
                &serde_json::json!({
                    "positions": layout.positions, "starts": layout.node_pts_start,
                    "counts": layout.node_pts_count, "components": layout.comp_ids,
                    "circular": layout.circular
                }),
            )
            .unwrap();
        }
    }

    #[test]
    fn large_connected_graph_stays_finite() {
        use Strand::Forward as F;
        let links: Vec<_> = (1..10000).map(|i| (i - 1, F, i, F)).collect();
        let graph = graph(&vec![200.0; 10000], &links);
        let start = std::time::Instant::now();
        let mut layout = Layout::new_with_graph(&graph);
        let init = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..5 {
            layout.step(&graph, &LayoutParams::default(), None);
        }
        eprintln!(
            "10k connected segments: init {:?}, 5 steps {:?}",
            init,
            start.elapsed()
        );
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
        assert_eq!(layout.iteration, 5);
    }

    #[test]
    fn bandage_initialization_is_settled() {
        use Strand::Forward as F;
        let graph = graph(
            &[400.0; 5],
            &[
                (0, F, 1, F),
                (0, F, 2, F),
                (1, F, 3, F),
                (2, F, 3, F),
                (3, F, 4, F),
            ],
        );
        let layout = Layout::new_with_graph(&graph);
        assert!(!layout.active_points.is_empty());
        assert!(
            layout.converged,
            "native FMMM must succeed without falling back"
        );
        assert_eq!(layout.iteration, 0);
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
        let min_y = layout
            .positions
            .iter()
            .map(|p| p[1])
            .fold(f32::INFINITY, f32::min);
        let max_y = layout
            .positions
            .iter()
            .map(|p| p[1])
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(max_y - min_y > 100.0);
    }

    #[test]
    fn wide_branching_levels_are_compact() {
        use Strand::Forward as F;
        let links: Vec<_> = (1..1000).map(|i| (0, F, i, F)).collect();
        let graph = graph(&vec![200.0; 1000], &links);
        let layout = Layout::new_with_graph(&graph);
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for p in &layout.positions {
            for d in 0..2 {
                lo[d] = lo[d].min(p[d]);
                hi[d] = hi[d].max(p[d]);
            }
        }
        let ratio = (hi[0] - lo[0]) / (hi[1] - lo[1]);
        assert!((0.3..3.0).contains(&ratio), "aspect ratio {ratio}");
    }

    #[test]
    fn empty_and_many_components() {
        let empty = graph(&[], &[]);
        let mut layout = Layout::new_with_graph(&empty);
        layout.step(&empty, &LayoutParams::default(), None);
        let graph = graph(&vec![200.0; 10000], &[]);
        let start = std::time::Instant::now();
        let mut layout = Layout::new_with_graph(&graph);
        let init = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..5 {
            layout.step(&graph, &LayoutParams::default(), None);
        }
        eprintln!(
            "10k components: init {:?}, 5 steps {:?}",
            init,
            start.elapsed()
        );
        assert_eq!(layout.positions.len(), 20000);
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
    }
}
