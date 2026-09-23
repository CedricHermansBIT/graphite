use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::{Arc, Mutex};

use crate::gfa::Strand;
use crate::graph::{EdgeKind, ViewGraph};
use crate::rust_layout;

pub type Pos2 = [f32; 2];

#[cfg(feature = "ogdf")]
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

/// Bandage normalises the drawn graph to a mean node length of roughly 40,
/// samples long nodes every 20 units and uses 5-unit graph links. Graphite
/// keeps its own visible length scale, so convert those ratios back into
/// display units for the current graph.
const BANDAGE_MEAN_NODE_LENGTH: f32 = 40.0;
const BANDAGE_MIN_TOTAL_GRAPH_LENGTH: f32 = 500.0;
const BANDAGE_NODE_SEGMENT_LENGTH: f32 = 20.0;
const GRAPH_EDGE_RATIO: f32 = 5.0 / BANDAGE_NODE_SEGMENT_LENGTH;

/// Maximum physics nodes per GFA segment (caps very long contigs).
const MAX_PTS: usize = 64;

/// Stiffness for internal (within-segment) adjacent springs.
const SPRING_INTERNAL: f32 = 0.55;

/// Stiffness for bending springs (skip-one: i↔i+2). Resists chain folding.
/// Rest length = 2 × segment_spacing keeps the chain straight.
const SPRING_BEND: f32 = 0.45;

/// Across a known linear GFA junction, connect the two interior points as a
/// long skip spring. This preserves a gentle path through segment boundaries
/// after interactive dragging without making the component rigid.
const SPRING_LINEAR_BEND: f32 = 0.30;

/// Stiffness for graph-link springs — deliberately weak so repulsion can compete.
const SPRING_LINK: f32 = 0.05;

/// Extra damping during a grab. This scales each non-pinned displacement before
/// the normal temperature clamp, reducing oscillation without making followers
/// feel stuck.
const DRAG_DAMPING_SCALE: f32 = 0.75;

/// Low-pass the force during an active grab. Branch points can receive strong
/// competing spring forces; blending consecutive forces prevents those forces
/// from flipping direction abruptly while keeping sustained motion responsive.
const DRAG_FORCE_SMOOTHING: f32 = 0.35;

/// Keep the per-frame force response during a grab at the same movement scale
/// as the start of ordinary relaxation. The grabbed point itself still follows
/// the cursor exactly, but connected pieces no longer jump away under a much
/// larger drag-only temperature.
const DRAG_MOVE_LIMIT_SCALE: f32 = 0.25;

/// Bandage's "nearby pieces" drag uses an index-distance falloff with a default
/// strength of 100. Use the same curve for the dragged contig so it bends around
/// the grabbed point instead of translating as a rigid polyline.
const DRAG_FALLOFF_STRENGTH: f32 = 100.0;

/// Interactive manipulation uses Position-Based Dynamics rather than the
/// force-directed layout solver. Distance constraints are intentionally hard:
/// softness comes from bending and free graph junctions, not from stretching.
const PBD_ITERATIONS: usize = 12;
const PBD_BEND_STIFFNESS: f32 = 0.18;
const PBD_RING_AREA_STIFFNESS: f32 = 0.08;

#[derive(Clone, Copy, Debug)]
struct PbdDistanceConstraint {
    a: usize,
    b: usize,
    rest: f32,
}

#[derive(Clone, Copy, Debug)]
struct PbdBendConstraint {
    a: usize,
    b: usize,
    rest: f32,
}

#[derive(Clone, Copy, Debug)]
struct HeapState {
    cost: f32,
    node: usize,
}

impl PartialEq for HeapState {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.cost.to_bits() == other.cost.to_bits()
    }
}
impl Eq for HeapState {}
impl Ord for HeapState {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.node.cmp(&self.node))
    }
}
impl PartialOrd for HeapState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn polygon_signed_area(positions: &[Pos2], path: &[usize]) -> f32 {
    if path.len() < 3 {
        return 0.0;
    }
    let mut twice_area = 0.0_f32;
    for index in 0..path.len() {
        let a = positions[path[index]];
        let b = positions[path[(index + 1) % path.len()]];
        twice_area += a[0] * b[1] - b[0] * a[1];
    }
    twice_area * 0.5
}

fn preserve_ring_area(
    positions: &mut [Pos2],
    path: &[usize],
    target_area: f32,
    pinned: usize,
    stiffness: f32,
) {
    if path.len() < 3 || target_area.abs() <= 0.001 {
        return;
    }
    let current = polygon_signed_area(positions, path);
    if current.abs() <= 0.001 {
        return;
    }

    let mut center = [0.0_f32; 2];
    for &pi in path {
        center[0] += positions[pi][0];
        center[1] += positions[pi][1];
    }
    center[0] /= path.len() as f32;
    center[1] /= path.len() as f32;

    let desired_scale = (target_area.abs() / current.abs()).sqrt();
    let scale = (1.0 + (desired_scale - 1.0) * stiffness).clamp(0.95, 1.05);
    for &pi in path {
        if pi == pinned {
            continue;
        }
        positions[pi][0] = center[0] + (positions[pi][0] - center[0]) * scale;
        positions[pi][1] = center[1] + (positions[pi][1] - center[1]) * scale;
    }
}

fn bandage_equivalent_spacing(graph: &ViewGraph) -> f32 {
    if graph.nodes.is_empty() {
        return BANDAGE_NODE_SEGMENT_LENGTH;
    }
    let total_visual = graph
        .nodes
        .iter()
        .map(|node| node.visual_len.max(1.0) as f64)
        .sum::<f64>();
    let target_total = ((graph.nodes.len() as f64) * BANDAGE_MEAN_NODE_LENGTH as f64)
        .max(BANDAGE_MIN_TOTAL_GRAPH_LENGTH as f64);
    let scale_to_bandage = target_total / total_visual.max(1.0);
    (BANDAGE_NODE_SEGMENT_LENGTH / scale_to_bandage as f32).max(0.5)
}

fn satisfy_distance_constraint(
    positions: &mut [Pos2],
    a: usize,
    b: usize,
    wanted: f32,
    pinned: usize,
    stiffness: f32,
) {
    let pa = positions[a];
    let pb = positions[b];
    let dx = pb[0] - pa[0];
    let dy = pb[1] - pa[1];
    let distance = dx.hypot(dy);
    if distance <= 0.001 {
        return;
    }

    let error = (distance - wanted) * stiffness;
    let correction = [dx / distance * error, dy / distance * error];

    match (a == pinned, b == pinned) {
        (true, false) => {
            positions[b][0] -= correction[0];
            positions[b][1] -= correction[1];
        }
        (false, true) => {
            positions[a][0] += correction[0];
            positions[a][1] += correction[1];
        }
        (false, false) => {
            positions[a][0] += correction[0] * 0.5;
            positions[a][1] += correction[1] * 0.5;
            positions[b][0] -= correction[0] * 0.5;
            positions[b][1] -= correction[1] * 0.5;
        }
        (true, true) => {}
    }
}

fn layout_hash(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn unit_hash(value: u64) -> f32 {
    ((layout_hash(value) >> 40) as u32) as f32 / 16_777_215.0
}

fn seed_linear_component(
    comp: &[usize],
    component_index: usize,
    graph: &ViewGraph,
    ends: &[Vec<usize>],
    node_pts_start: &[usize],
    node_pts_count: &[usize],
    positions: &mut [Pos2],
    physics_spacing: f32,
    graph_edge_desired: f32,
) {
    let Some(mut entry) = comp
        .iter()
        .flat_map(|&node| [2 * node, 2 * node + 1])
        .filter(|&endpoint| ends[endpoint].is_empty())
        .min()
    else {
        return;
    };

    let seed = layout_hash(
        (component_index as u64).rotate_left(23)
            ^ entry as u64
            ^ (comp.len() as u64).rotate_left(41),
    );
    let phase = unit_hash(seed ^ 0xD2B7_4407_B1CE_6E93) * std::f32::consts::TAU;
    let phase2 = unit_hash(seed ^ 0x69D5_7FC8_A2E4_7301) * std::f32::consts::TAU;
    let turn_amplitude = 0.14 + 0.10 * unit_hash(seed ^ 0xA24B_AED4_963E_E407);
    let secondary_amplitude = 0.025 + 0.035 * unit_hash(seed ^ 0x9FB2_1C65_1E98_DF25);
    let wavelength =
        physics_spacing * (7.0 + 3.0 * unit_hash(seed ^ 0x31D0_8C59_EA22_4A9B));

    let mut position = [0.0_f32, 0.0_f32];
    let mut distance = 0.0_f32;
    let advance = |step: f32, position: &mut Pos2, distance: &mut f32| {
        if step <= 0.0 {
            return;
        }
        let midpoint = *distance + step * 0.5;
        let theta = turn_amplitude
            * (std::f32::consts::TAU * midpoint / wavelength + phase).sin()
            + secondary_amplitude
                * (std::f32::consts::TAU * midpoint / (wavelength * 0.57) + phase2).sin();
        position[0] += step * theta.cos();
        position[1] += step * theta.sin();
        *distance += step;
    };

    let mut visited = 0usize;
    while visited < comp.len() {
        let node = entry / 2;
        let count = node_pts_count[node];
        let start = node_pts_start[node];
        let segment_step = if count > 1 {
            graph.nodes[node].visual_len / (count - 1) as f32
        } else {
            0.0
        };

        for j in 0..count {
            let index = if entry % 2 == 0 { j } else { count - 1 - j };
            positions[start + index] = position;
            if j + 1 < count {
                advance(segment_step, &mut position, &mut distance);
            }
        }

        visited += 1;
        let exit = entry ^ 1;
        if ends[exit].is_empty() {
            break;
        }
        advance(graph_edge_desired, &mut position, &mut distance);
        entry = ends[exit][0];
    }
}

// ── Layout ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBackend {
    #[cfg(feature = "ogdf")]
    Bandage,
    Rust,
}

impl LayoutBackend {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            #[cfg(feature = "ogdf")]
            "bandage" | "ogdf" => Some(Self::Bandage),
            "rust" => Some(Self::Rust),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            #[cfg(feature = "ogdf")]
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
    linear: Vec<bool>,
    fixed: Vec<bool>,
    /// Generic PBD interaction graph used for linear, branched, circular and
    /// isolated components. Distance constraints include both within-contig
    /// links and GFA links; bend constraints exist only inside contigs.
    pbd_distances: Vec<PbdDistanceConstraint>,
    pbd_bends: Vec<PbdBendConstraint>,
    pbd_component_distances: Vec<Vec<usize>>,
    pbd_component_bends: Vec<Vec<usize>>,
    pbd_particles: Vec<Vec<usize>>,
    pbd_adjacency: Vec<Vec<(usize, f32)>>,
    pbd_ring_paths: Vec<Vec<usize>>,
    pbd_ring_area: Vec<f32>,
    drag_anchor: Option<usize>,
    drag_lra: Vec<f32>,

    physics_spacing: f32,
    active_points: Vec<usize>,
    grid: GridIndex,
    user_positioned: bool,
    drag_component: Option<usize>,

    /// Reusable displacement buffer — zeroed at the start of each step.
    disp: Vec<Pos2>,

    /// Low-pass filtered drag force from the previous iteration. This smooths
    /// competing spring directions at branch points without reducing the
    /// long-range pull on connected segments.
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

    /// Return the physics point on a visible contig that is closest to the
    /// world-space cursor. Grab mode must use this rather than always taking
    /// the first point of the contig, otherwise every drag is effectively an
    /// endpoint drag even when the user clicks the middle of a segment.
    pub fn nearest_physics_point(&self, node: usize, pos: Pos2) -> Option<usize> {
        if node >= self.num_nodes() {
            return None;
        }
        let start = self.node_pts_start[node];
        let count = self.node_pts_count[node];
        (start..start + count).min_by(|&a, &b| {
            let da = {
                let dx = self.positions[a][0] - pos[0];
                let dy = self.positions[a][1] - pos[1];
                dx * dx + dy * dy
            };
            let db = {
                let dx = self.positions[b][0] - pos[0];
                let dy = self.positions[b][1] - pos[1];
                dx * dx + dy * dy
            };
            da.total_cmp(&db)
        })
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
        Self::new_with_graph_backend(graph, LayoutBackend::Rust)
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
                linear: Vec::new(),
                fixed: Vec::new(),
                pbd_distances: Vec::new(),
                pbd_bends: Vec::new(),
                pbd_component_distances: Vec::new(),
                pbd_component_bends: Vec::new(),
                pbd_particles: Vec::new(),
                pbd_adjacency: Vec::new(),
                pbd_ring_paths: Vec::new(),
                pbd_ring_area: Vec::new(),
                drag_anchor: None,
                drag_lra: Vec::new(),
                physics_spacing: BANDAGE_NODE_SEGMENT_LENGTH,
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

        let physics_spacing = bandage_equivalent_spacing(graph);
        let graph_edge_desired = physics_spacing * GRAPH_EDGE_RATIO;

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
        // Match Bandage's ceil(drawn_len / nodeSegmentLength) + 1 sampling,
        // translated back into Graphite's visible coordinate scale.
        let mut node_pts_count = vec![2usize; n];
        for ni in 0..n {
            let vl = graph.nodes[ni].visual_len;
            node_pts_count[ni] =
                ((vl / physics_spacing).ceil() as usize + 1).clamp(2, MAX_PTS);
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
        let linear: Vec<bool> = components
            .iter()
            .map(|comp| {
                let mut open_ends = 0usize;
                let no_branches = comp.iter().all(|&v| {
                    for endpoint in [2 * v, 2 * v + 1] {
                        open_ends += usize::from(ends[endpoint].is_empty());
                        if ends[endpoint].len() > 1 {
                            return false;
                        }
                    }
                    true
                });
                no_branches && open_ends == 2
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
                    .map(|&v| {
                        graph.nodes[v].visual_len.max(physics_spacing) + graph_edge_desired
                    })
                    .sum();
                let radius = circumference / std::f32::consts::TAU;
                let mut entry = comp[0] * 2;
                let mut distance = 0.0;
                for _ in 0..comp.len() {
                    let v = entry / 2;
                    let len = graph.nodes[v].visual_len.max(physics_spacing);
                    let count = node_pts_count[v];
                    for j in 0..count {
                        let angle = (distance + len * j as f32 / (count - 1) as f32) / radius;
                        let index = if entry % 2 == 0 { j } else { count - 1 - j };
                        positions[node_pts_start[v] + index] =
                            [radius * angle.cos(), radius * angle.sin()];
                    }
                    distance += len + graph_edge_desired;
                    entry = ends[entry ^ 1][0];
                }
            } else if linear[ci] {
                seed_linear_component(
                    comp,
                    ci,
                    graph,
                    &ends,
                    &node_pts_start,
                    &node_pts_count,
                    &mut positions,
                    physics_spacing,
                    graph_edge_desired,
                );
            } else {
                // Start branching components at a tip; use compact, length-aware BFS columns
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
                    .map(|(&w, &count)| {
                        (w + physics_spacing) * physics_spacing * 2.0 * count as f32
                    })
                    .sum();
                let max_rows =
                    (area.sqrt() / (physics_spacing * 2.0)).ceil().max(1.0) as usize;
                let mut x = vec![0.0; widths.len()];
                for d in 1..x.len() {
                    x[d] = x[d - 1]
                        + counts[d - 1].div_ceil(max_rows) as f32
                            * (widths[d - 1] + physics_spacing);
                }
                let mut rows = vec![0; widths.len()];
                for v in queue {
                    let d = depth[v];
                    let column = rows[d] / max_rows;
                    let y = ((rows[d] % max_rows) as f32
                        - (counts[d].min(max_rows) - 1) as f32 * 0.5)
                        * physics_spacing
                        * 2.0;
                    rows[d] += 1;
                    let count = node_pts_count[v];
                    for j in 0..count {
                        let index = if reverse[v] { count - 1 - j } else { j };
                        positions[node_pts_start[v] + index] = [
                            x[d] + column as f32 * (widths[d] + physics_spacing)
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

        // ── Generic PBD interaction topology ────────────────────────────────
        // This graph is independent of the FMMM/Rust layout solver. It is used
        // only while the user manipulates a component.
        let mut pbd_distances: Vec<PbdDistanceConstraint> = Vec::new();
        let mut pbd_bends: Vec<PbdBendConstraint> = Vec::new();
        let mut pbd_component_distances = vec![Vec::new(); components.len()];
        let mut pbd_component_bends = vec![Vec::new(); components.len()];
        let mut pbd_seen = HashSet::<(usize, usize)>::new();

        for node in 0..n {
            let ci = comp_ids[node];
            let start = node_pts_start[node];
            let count = node_pts_count[node];

            for j in 0..count.saturating_sub(1) {
                let a = start + j;
                let b = start + j + 1;
                let key = if a < b { (a, b) } else { (b, a) };
                if pbd_seen.insert(key) {
                    let index = pbd_distances.len();
                    pbd_distances.push(PbdDistanceConstraint { a, b, rest: 0.0 });
                    pbd_component_distances[ci].push(index);
                }
            }
            for j in 0..count.saturating_sub(2) {
                let index = pbd_bends.len();
                pbd_bends.push(PbdBendConstraint {
                    a: start + j,
                    b: start + j + 2,
                    rest: 0.0,
                });
                pbd_component_bends[ci].push(index);
            }
        }

        for edge in &graph.edges {
            if edge.from >= n || edge.to >= n {
                continue;
            }
            let a = match edge.from_strand {
                Strand::Forward => node_pts_start[edge.from] + node_pts_count[edge.from] - 1,
                Strand::Reverse => node_pts_start[edge.from],
            };
            let b = match edge.to_strand {
                Strand::Forward => node_pts_start[edge.to],
                Strand::Reverse => node_pts_start[edge.to] + node_pts_count[edge.to] - 1,
            };
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            if pbd_seen.insert(key) {
                let ci = comp_ids[edge.from];
                let index = pbd_distances.len();
                pbd_distances.push(PbdDistanceConstraint { a, b, rest: 0.0 });
                pbd_component_distances[ci].push(index);
            }
        }

        let pbd_particles: Vec<Vec<usize>> = components
            .iter()
            .map(|comp| {
                comp.iter()
                    .flat_map(|&node| {
                        let start = node_pts_start[node];
                        start..start + node_pts_count[node]
                    })
                    .collect()
            })
            .collect();

        // Circular components get an ordered closed path for a weak area
        // preservation constraint. Branched cyclic components deliberately do
        // not: their topology is handled by generic graph constraints instead.
        let mut pbd_ring_paths = vec![Vec::new(); components.len()];
        for (ci, comp) in components.iter().enumerate() {
            if !circular[ci] || comp.is_empty() {
                continue;
            }
            let mut entry = comp[0] * 2;
            let path = &mut pbd_ring_paths[ci];
            for _ in 0..comp.len() {
                let node = entry / 2;
                let start = node_pts_start[node];
                let count = node_pts_count[node];
                if entry % 2 == 0 {
                    path.extend(start..start + count);
                } else {
                    path.extend((start..start + count).rev());
                }
                entry = ends[entry ^ 1][0];
            }
        }

        let active_points: Vec<_> = (0..total_pts)
            .filter(|&pi| !fixed[comp_ids[phys_to_node[pi] as usize]])
            .collect();

        // ── Build spring list (SoA for cache efficiency) ──────────────────────
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

            if linear[comp_ids[u]] && u != v {
                let u_start = node_pts_start[u];
                let v_start = node_pts_start[v];
                let u_inner = if pu == u_start { pu + 1 } else { pu - 1 };
                let v_inner = if pv == v_start { pv + 1 } else { pv - 1 };
                let u_step = graph.nodes[u].visual_len / (node_pts_count[u] - 1) as f32;
                let v_step = graph.nodes[v].visual_len / (node_pts_count[v] - 1) as f32;
                push_spring(
                    u_inner,
                    v_inner,
                    u_step + desired + v_step,
                    SPRING_LINEAR_BEND,
                );
            }
        }

        let component_count = components.len();
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
            linear,
            fixed,
            pbd_distances,
            pbd_bends,
            pbd_component_distances,
            pbd_component_bends,
            pbd_particles,
            pbd_adjacency: vec![Vec::new(); total_pts],
            pbd_ring_paths,
            pbd_ring_area: vec![0.0; component_count],
            drag_anchor: None,
            drag_lra: vec![f32::INFINITY; total_pts],
            physics_spacing,
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
            #[cfg(feature = "ogdf")]
            LayoutBackend::Bandage => layout.seed_with_bandage(graph),
            LayoutBackend::Rust => layout.seed_with_rust(graph),
        };
        layout.orient_tall_components();
        layout.pack_components();
        layout.refresh_pbd_rest_geometry();
        layout
    }

    /// Use Bandage's bundled OGDF FMMM implementation on connected, non-ring
    /// polylines. Isolated contigs and explicit circles need no force solve.
    #[cfg(feature = "ogdf")]
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
            if self.fixed[self.comp_ids[v]] || self.linear[self.comp_ids[v]] {
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
                continue;
            }
            from.push(a);
            to.push(b);
            lengths.push(self.springs_desired[i]);
        }
        if samples.is_empty() {
            return true;
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
            if self.fixed[self.comp_ids[v]] || self.linear[self.comp_ids[v]] {
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
            let a_pi = self.springs_a[i] as usize;
            let b_pi = self.springs_b[i] as usize;
            let a = indices[a_pi];
            let b = indices[b_pi];
            if a == u32::MAX || b == u32::MAX {
                // Linear and fixed components are deliberately omitted from
                // the generic solver. Their graph-link endpoints therefore
                // have no sampled index here.
                let a_component = self.comp_ids[self.phys_to_node[a_pi] as usize];
                let b_component = self.comp_ids[self.phys_to_node[b_pi] as usize];
                if self.linear[a_component]
                    || self.fixed[a_component]
                    || self.linear[b_component]
                    || self.fixed[b_component]
                {
                    continue;
                }
                log::warn!("Rust layout skipped an unresolved sampled spring endpoint");
                continue;
            }
            from.push(a);
            to.push(b);
            lengths.push(self.springs_desired[i]);
        }

        if samples.is_empty() {
            return true;
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

    /// Rotate tall solved components by 90 degrees before packing. Bandage
    /// performs a more expensive angle sweep for the same purpose; this cheap
    /// pass keeps elongated components horizontal in the overview.
    fn orient_tall_components(&mut self) {
        for ci in 0..self.components.len() {
            let mut lo = [f32::INFINITY; 2];
            let mut hi = [f32::NEG_INFINITY; 2];
            for &v in &self.components[ci] {
                for p in self.pts(v) {
                    lo[0] = lo[0].min(p[0]);
                    lo[1] = lo[1].min(p[1]);
                    hi[0] = hi[0].max(p[0]);
                    hi[1] = hi[1].max(p[1]);
                }
            }
            if hi[1] - lo[1] <= hi[0] - lo[0] {
                continue;
            }
            for &v in &self.components[ci] {
                let start = self.node_pts_start[v];
                let count = self.node_pts_count[v];
                for p in &mut self.positions[start..start + count] {
                    let old_x = p[0];
                    p[0] = -p[1];
                    p[1] = old_x;
                }
            }
        }
    }

    /// Pack actual polyline bounds, including long singletons and circles.
    fn pack_components(&mut self) {
        let gap = self.physics_spacing * 3.0;
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

    /// Capture the final layout geometry as the rest state for interactive
    /// PBD manipulation. This runs after FMMM/Rust placement, orientation and
    /// packing so the first grab never snaps to a different spring geometry.
    fn refresh_pbd_rest_geometry(&mut self) {
        let positions = &self.positions;
        for constraint in &mut self.pbd_distances {
            let a = positions[constraint.a];
            let b = positions[constraint.b];
            constraint.rest = (b[0] - a[0]).hypot(b[1] - a[1]).max(0.001);
        }
        for constraint in &mut self.pbd_bends {
            let a = positions[constraint.a];
            let b = positions[constraint.b];
            constraint.rest = (b[0] - a[0]).hypot(b[1] - a[1]).max(0.001);
        }

        for neighbours in &mut self.pbd_adjacency {
            neighbours.clear();
        }
        for constraint in &self.pbd_distances {
            self.pbd_adjacency[constraint.a].push((constraint.b, constraint.rest));
            self.pbd_adjacency[constraint.b].push((constraint.a, constraint.rest));
        }

        for ci in 0..self.pbd_ring_paths.len() {
            self.pbd_ring_area[ci] =
                polygon_signed_area(&self.positions, &self.pbd_ring_paths[ci]);
        }
    }

    /// Start or retarget a PBD grab and compute Long Range Attachment limits.
    /// The LRA radius for every particle is its shortest rest-length path from
    /// the grabbed particle through the component's constraint graph.
    fn prepare_pbd_drag(&mut self, pi: usize) -> Option<usize> {
        if pi >= self.positions.len() {
            return None;
        }
        let node = self.phys_to_node[pi] as usize;
        let ci = self.comp_ids[node];
        if self.drag_anchor == Some(pi) && self.drag_component == Some(ci) {
            return Some(ci);
        }

        self.drag_component = Some(ci);
        self.drag_anchor = Some(pi);
        self.drag_lra.fill(f32::INFINITY);
        self.drag_lra[pi] = 0.0;

        let mut heap = BinaryHeap::new();
        heap.push(HeapState { cost: 0.0, node: pi });

        while let Some(HeapState { cost, node }) = heap.pop() {
            if cost > self.drag_lra[node] {
                continue;
            }
            for &(next, weight) in &self.pbd_adjacency[node] {
                let next_node = self.phys_to_node[next] as usize;
                if self.comp_ids[next_node] != ci {
                    continue;
                }
                let next_cost = cost + weight;
                if next_cost < self.drag_lra[next] {
                    self.drag_lra[next] = next_cost;
                    heap.push(HeapState {
                        cost: next_cost,
                        node: next,
                    });
                }
            }
        }

        Some(ci)
    }

    fn project_lra(&mut self, ci: usize, pinned: usize) {
        let anchor = self.positions[pinned];
        for &pi in &self.pbd_particles[ci] {
            if pi == pinned {
                continue;
            }
            let max_distance = self.drag_lra[pi];
            if !max_distance.is_finite() {
                continue;
            }
            let dx = self.positions[pi][0] - anchor[0];
            let dy = self.positions[pi][1] - anchor[1];
            let distance = dx.hypot(dy);
            if distance > max_distance && distance > 0.001 {
                let scale = max_distance / distance;
                self.positions[pi] = [anchor[0] + dx * scale, anchor[1] + dy * scale];
            }
        }
    }

    /// Generic Position-Based Dynamics manipulation for every component type.
    /// Hard distance constraints prevent stretch, LRA prevents slow global
    /// extension on long/branched graphs, soft skip-one constraints provide
    /// bend resistance inside contigs, and circular components weakly preserve
    /// their enclosed area. GFA junctions have no bend constraint and therefore
    /// remain flexible hinges.
    fn drag_pbd_to(&mut self, pos: Pos2, pi: usize) -> bool {
        let Some(ci) = self.prepare_pbd_drag(pi) else {
            return false;
        };

        self.user_positioned = true;

        for _ in 0..PBD_ITERATIONS {
            self.positions[pi] = pos;

            // Global anti-stretch first, so distant branches respond
            // immediately instead of waiting for local corrections to travel.
            self.project_lra(ci, pi);

            for &index in &self.pbd_component_distances[ci] {
                let c = self.pbd_distances[index];
                satisfy_distance_constraint(
                    &mut self.positions,
                    c.a,
                    c.b,
                    c.rest,
                    pi,
                    1.0,
                );
            }
            for &index in self.pbd_component_distances[ci].iter().rev() {
                let c = self.pbd_distances[index];
                satisfy_distance_constraint(
                    &mut self.positions,
                    c.a,
                    c.b,
                    c.rest,
                    pi,
                    1.0,
                );
            }

            for &index in &self.pbd_component_bends[ci] {
                let c = self.pbd_bends[index];
                satisfy_distance_constraint(
                    &mut self.positions,
                    c.a,
                    c.b,
                    c.rest,
                    pi,
                    PBD_BEND_STIFFNESS,
                );
            }

            if !self.pbd_ring_paths[ci].is_empty() {
                preserve_ring_area(
                    &mut self.positions,
                    &self.pbd_ring_paths[ci],
                    self.pbd_ring_area[ci],
                    pi,
                    PBD_RING_AREA_STIFFNESS,
                );
            }

            self.positions[pi] = pos;
            self.project_lra(ci, pi);
        }

        self.positions[pi] = pos;
        true
    }

    /// Apply the same drag behavior used by the background layout worker to
    /// the UI snapshot for immediate feedback. Keeping both paths identical is
    /// important: using generic drag_to() in the UI while the worker applies
    /// rope constraints makes the two layouts fight and causes visible jumps.
    pub fn drag_preview_to(&mut self, pos: Pos2, pi: usize) {
        if !self.drag_pbd_to(pos, pi) {
            self.drag_to(pos, pi);
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
            // Match Bandage's "nearby pieces" feel for non-linear components:
            // the grabbed physics point follows the cursor exactly, while
            // progressively more distant points on the same contig move less.
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
            if !self.drag_pbd_to(pos, pi) {
                self.drag_to(pos, pi);
            }
            for force in &mut self.prev_drag_force {
                *force = [0.0, 0.0];
            }
            self.iteration += 1;
            self.converged = false;
            return;
        } else if self.drag_component.take().is_some() {
            // PBD already leaves a valid constraint-satisfying geometry. Keep
            // the manual placement instead of waking FMMM/FR on release.
            self.drag_anchor = None;
            self.drag_lra.fill(f32::INFINITY);
            for force in &mut self.prev_drag_force {
                *force = [0.0, 0.0];
            }
            self.iteration += 1;
            self.converged = true;
            return;
        }

        if self.active_points.is_empty() || self.drag_component.is_some_and(|ci| self.fixed[ci]) {
            self.iteration += 1;
            self.converged = attractor.is_none();
            return;
        }
        let k = self.physics_spacing;
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
                    dpv[0] += k2 * dx / dist2;
                    dpv[1] += k2 * dy / dist2;
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
            let stiff = self.springs_stiff[i];
            if attractor.is_some() && stiff == SPRING_LINEAR_BEND {
                // The long cross-junction bend spring exists to restore the
                // overview shape after editing. Applying it while the pointer
                // is held makes neighbouring linear segments feel unnaturally
                // rubber-banded to the grabbed contig.
                continue;
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
            let (dx, dy) = if attractor.is_some() {
                let previous = self.prev_drag_force[pi];
                let filtered = [
                    previous[0] + (raw_dx - previous[0]) * DRAG_FORCE_SMOOTHING,
                    previous[1] + (raw_dy - previous[1]) * DRAG_FORCE_SMOOTHING,
                ];
                self.prev_drag_force[pi] = filtered;
                (
                    filtered[0] * DRAG_DAMPING_SCALE,
                    filtered[1] * DRAG_DAMPING_SCALE,
                )
            } else {
                self.prev_drag_force[pi] = [0.0, 0.0];
                (raw_dx, raw_dy)
            };
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

    fn assert_lra_limits(layout: &Layout, ci: usize, anchor: Pos2) {
        for &pi in &layout.pbd_particles[ci] {
            let limit = layout.drag_lra[pi];
            if !limit.is_finite() {
                continue;
            }
            let dx = layout.positions[pi][0] - anchor[0];
            let dy = layout.positions[pi][1] - anchor[1];
            assert!(
                dx.hypot(dy) <= limit * 1.001 + 0.01,
                "LRA limit exceeded for particle {pi}"
            );
        }
    }

    fn assert_pbd_strain_bounded(layout: &Layout, ci: usize, max_relative_error: f32) {
        for &index in &layout.pbd_component_distances[ci] {
            let c = layout.pbd_distances[index];
            let a = layout.positions[c.a];
            let b = layout.positions[c.b];
            let length = (b[0] - a[0]).hypot(b[1] - a[1]);
            let relative_error = (length - c.rest).abs() / c.rest.max(0.001);
            assert!(
                relative_error <= max_relative_error,
                "PBD constraint strain {:.1}% exceeds {:.1}%: rest {}, current {length}",
                relative_error * 100.0,
                max_relative_error * 100.0,
                c.rest
            );
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
        let expected_link = bandage_equivalent_spacing(&graph) * GRAPH_EDGE_RATIO;
        for e in &graph.edges {
            let a = layout.strand_endpoint(e.from, e.from_strand);
            let b = match e.to_strand {
                F => layout.start(e.to),
                R => layout.end(e.to),
            };
            let distance = (a[0] - b[0]).hypot(a[1] - b[1]);
            assert!(
                distance <= expected_link * 1.05 + 0.01,
                "ring link gap {distance} exceeds expected layout gap {expected_link}"
            );
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
    fn grab_uses_nearest_physics_point_not_contig_start() {
        let graph = graph(&[4000.0], &[]);
        let layout = Layout::new_with_graph(&graph);
        let start = layout.node_pts_start[0];
        let count = layout.node_pts_count[0];
        assert!(count >= 3);

        let middle = start + count / 2;
        let target = layout.positions[middle];
        assert_eq!(layout.nearest_physics_point(0, target), Some(middle));
    }

    #[test]
    fn pbd_linear_drag_respects_lra_and_local_lengths() {
        use Strand::Forward as F;
        let graph = graph(&[1200.0, 1200.0, 1200.0], &[(0, F, 1, F), (1, F, 2, F)]);
        let mut layout = Layout::new_with_graph(&graph);
        let ci = layout.comp_ids[1];
        assert!(layout.linear[ci]);

        let grabbed = layout.node_pts_start[1] + layout.node_pts_count[1] / 2;
        let target = [
            layout.positions[grabbed][0] + 600.0,
            layout.positions[grabbed][1] + 220.0,
        ];
        assert!(layout.drag_pbd_to(target, grabbed));
        assert_eq!(layout.positions[grabbed], target);

        assert_lra_limits(&layout, ci, target);
        // PBD is iterative: local constraints need not be exact after a fixed
        // interaction budget. The important invariants are strict LRA global
        // anti-stretch plus bounded local strain, not 3% convergence.
        assert_pbd_strain_bounded(&layout, ci, 0.12);
    }

    #[test]
    fn pbd_branched_drag_moves_followers_without_exploding() {
        use Strand::Forward as F;
        let graph = graph(
            &[1200.0, 1200.0, 1200.0, 1200.0],
            &[(0, F, 1, F), (0, F, 2, F), (0, F, 3, F)],
        );
        let mut layout = Layout::new_with_graph(&graph);
        let ci = layout.comp_ids[0];
        assert!(!layout.linear[ci]);

        let follower_before = layout.center(3);
        let grabbed = layout.node_pts_start[0] + layout.node_pts_count[0] / 2;
        let target = [
            layout.positions[grabbed][0] + 500.0,
            layout.positions[grabbed][1] + 200.0,
        ];
        assert!(layout.drag_pbd_to(target, grabbed));
        assert_eq!(layout.positions[grabbed], target);
        assert!(layout.positions.iter().flatten().all(|value| value.is_finite()));

        let follower_after = layout.center(3);
        assert!(
            (follower_after[0] - follower_before[0])
                .hypot(follower_after[1] - follower_before[1])
                > 0.1,
            "a connected branch should follow the grab"
        );

        assert_lra_limits(&layout, ci, target);
        assert_pbd_strain_bounded(&layout, ci, 0.12);
    }

    #[test]
    fn pbd_circular_drag_preserves_lengths_and_open_area() {
        use Strand::Forward as F;
        let graph = graph(
            &[900.0, 900.0, 900.0],
            &[(0, F, 1, F), (1, F, 2, F), (2, F, 0, F)],
        );
        let mut layout = Layout::new_with_graph(&graph);
        let ci = layout.comp_ids[0];
        assert!(layout.circular[ci]);
        let initial_area = layout.pbd_ring_area[ci].abs();
        assert!(initial_area > 0.01);

        let grabbed = layout.node_pts_start[0] + layout.node_pts_count[0] / 2;
        let target = [
            layout.positions[grabbed][0] + 250.0,
            layout.positions[grabbed][1] - 180.0,
        ];
        assert!(layout.drag_pbd_to(target, grabbed));
        assert_eq!(layout.positions[grabbed], target);

        assert_lra_limits(&layout, ci, target);
        assert_pbd_strain_bounded(&layout, ci, 0.12);

        let area = polygon_signed_area(&layout.positions, &layout.pbd_ring_paths[ci]).abs();
        assert!(
            area > initial_area * 0.35,
            "ring collapsed too far: area {area}, initial {initial_area}"
        );
    }

    #[test]
    fn drag_force_filter_smooths_direction_reversal() {
        let previous = [100.0_f32, 0.0];
        let raw = [-100.0_f32, 0.0];
        let filtered = [
            previous[0] + (raw[0] - previous[0]) * DRAG_FORCE_SMOOTHING,
            previous[1] + (raw[1] - previous[1]) * DRAG_FORCE_SMOOTHING,
        ];

        assert!(
            filtered[0] > 0.0,
            "one opposing sample should not immediately flip the filtered force"
        );
        assert!(filtered[0].abs() < previous[0].abs());
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

    #[cfg(feature = "ogdf")]
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
        let layout = Layout::new_with_graph_backend(&graph, LayoutBackend::Bandage);
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
        assert_eq!(
            layout.positions.len(),
            layout.node_pts_count.iter().sum::<usize>()
        );
        assert!(
            layout.positions.len() >= 20_000,
            "each visible contig should retain at least its two endpoints"
        );
        assert!(layout.positions.iter().flatten().all(|v| v.is_finite()));
    }
}
