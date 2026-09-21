use ahash::AHashMap;
use rayon::prelude::*;

pub type Pos2 = [f32; 2];

const COARSE_TARGET: usize = 256;
const MAX_LEVELS: usize = 20;
const THETA: f32 = 0.72;
const EPSILON: f32 = 1.0e-4;

#[derive(Clone, Copy, Debug)]
struct Edge {
    a: u32,
    b: u32,
    desired: f32,
    weight: f32,
}

#[derive(Clone)]
struct Level {
    node_count: usize,
    edges: Vec<Edge>,
    masses: Vec<f32>,
    to_coarse: Option<Vec<u32>>,
}

struct ComponentWork {
    global_nodes: Vec<usize>,
    edges: Vec<Edge>,
    initial_positions: Vec<Pos2>,
}

struct ComponentResult {
    global_nodes: Vec<usize>,
    positions: Vec<Pos2>,
}

/// Specialized initial layout for Graphite's reduced assembly-graph representation.
///
/// Each connected component is solved independently. Large components use a
/// deterministic multilevel hierarchy with Barnes-Hut repulsion and weighted
/// spring attraction. The result is intentionally only an initial placement;
/// Graphite still owns component packing and interactive refinement.
pub fn initial_layout(
    node_count: usize,
    from: &[u32],
    to: &[u32],
    lengths: &[f32],
    initial_positions: &[Pos2],
    output: &mut [Pos2],
) -> Result<(), &'static str> {
    if from.len() != to.len()
        || from.len() != lengths.len()
        || initial_positions.len() != node_count
        || output.len() != node_count
    {
        return Err("layout input buffers have inconsistent lengths");
    }
    if node_count == 0 {
        return Ok(());
    }

    let mut edges = Vec::with_capacity(from.len());
    for ((&a, &b), &desired) in from.iter().zip(to).zip(lengths) {
        if a as usize >= node_count || b as usize >= node_count {
            return Err("layout edge endpoint is out of range");
        }
        if a == b {
            continue;
        }
        edges.push(Edge {
            a,
            b,
            desired: desired.max(1.0),
            weight: 1.0,
        });
    }

    let components = split_components(node_count, &edges, initial_positions);
    let solved: Vec<ComponentResult> = components
        .into_par_iter()
        .map(solve_component)
        .collect();

    for result in solved {
        for (local, global) in result.global_nodes.into_iter().enumerate() {
            output[global] = result.positions[local];
        }
    }

    if output.iter().flatten().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err("Rust layout produced non-finite coordinates")
    }
}

fn split_components(
    node_count: usize,
    edges: &[Edge],
    initial_positions: &[Pos2],
) -> Vec<ComponentWork> {
    let mut dsu = DisjointSet::new(node_count);
    for edge in edges {
        dsu.union(edge.a as usize, edge.b as usize);
    }

    let mut root_to_component = AHashMap::new();
    let mut global_nodes: Vec<Vec<usize>> = Vec::new();
    let mut node_component = vec![0usize; node_count];

    for node in 0..node_count {
        let root = dsu.find(node);
        let component = match root_to_component.get(&root).copied() {
            Some(component) => component,
            None => {
                let component = global_nodes.len();
                root_to_component.insert(root, component);
                global_nodes.push(Vec::new());
                component
            }
        };
        node_component[node] = component;
        global_nodes[component].push(node);
    }

    let mut global_to_local = vec![0u32; node_count];
    for nodes in &global_nodes {
        for (local, &global) in nodes.iter().enumerate() {
            global_to_local[global] = local as u32;
        }
    }

    let mut component_edges = vec![Vec::new(); global_nodes.len()];
    for edge in edges {
        let component = node_component[edge.a as usize];
        debug_assert_eq!(component, node_component[edge.b as usize]);
        component_edges[component].push(Edge {
            a: global_to_local[edge.a as usize],
            b: global_to_local[edge.b as usize],
            desired: edge.desired,
            weight: edge.weight,
        });
    }

    global_nodes
        .into_iter()
        .zip(component_edges)
        .map(|(global_nodes, edges)| {
            let initial_positions = global_nodes
                .iter()
                .map(|&global| initial_positions[global])
                .collect();
            ComponentWork {
                global_nodes,
                edges,
                initial_positions,
            }
        })
        .collect()
}

fn solve_component(work: ComponentWork) -> ComponentResult {
    let node_count = work.global_nodes.len();
    let positions = match node_count {
        0 => Vec::new(),
        1 => vec![[0.0, 0.0]],
        2 => {
            let length = work.edges.first().map_or(100.0, |edge| edge.desired);
            vec![[-0.5 * length, 0.0], [0.5 * length, 0.0]]
        }
        _ => solve_multilevel(node_count, work.edges, work.initial_positions),
    };
    ComponentResult {
        global_nodes: work.global_nodes,
        positions,
    }
}

fn solve_multilevel(
    node_count: usize,
    edges: Vec<Edge>,
    initial_positions: Vec<Pos2>,
) -> Vec<Pos2> {
    let mut levels = vec![Level {
        node_count,
        edges,
        masses: vec![1.0; node_count],
        to_coarse: None,
    }];
    let mut level_seeds = vec![initial_positions];

    while levels
        .last()
        .is_some_and(|level| level.node_count > COARSE_TARGET)
        && levels.len() < MAX_LEVELS
    {
        let current = levels.last().expect("level exists");
        let current_seed = level_seeds.last().expect("seed level exists");
        let (coarse, map, coarse_seed) = coarsen(current, current_seed);
        if coarse.node_count >= current.node_count.saturating_sub(current.node_count / 20) {
            break;
        }
        levels.last_mut().expect("level exists").to_coarse = Some(map);
        levels.push(coarse);
        level_seeds.push(coarse_seed);
    }

    // The coarsest level must break the collinear symmetry of long assembly
    // paths. Bandage/OGDF also starts its coarsest force solve from a random
    // placement. We use a deterministic pseudo-random square so repeated runs
    // remain reproducible, then restore local topology while prolongating.
    let coarsest = levels.last().expect("at least one level");
    let coarse_scale = mean_edge_length(coarsest).max(20.0);
    let mut positions = deterministic_coarse_seed(coarsest.node_count, coarse_scale);
    relax(
        coarsest,
        &mut positions,
        iterations_for(coarsest.node_count, true),
    );

    for level_index in (0..levels.len().saturating_sub(1)).rev() {
        let level = &levels[level_index];
        let map = level
            .to_coarse
            .as_ref()
            .expect("fine multilevel level must map to its parent");
        let fine_seed = &level_seeds[level_index];
        let parent_seed = &level_seeds[level_index + 1];
        let scale = mean_edge_length(level).max(20.0);
        let mut fine = vec![[0.0; 2]; level.node_count];
        for node in 0..level.node_count {
            let parent_index = map[node] as usize;
            let parent = positions[parent_index];
            let local_offset = [
                fine_seed[node][0] - parent_seed[parent_index][0],
                fine_seed[node][1] - parent_seed[parent_index][1],
            ];
            let jitter = deterministic_jitter(node, scale * 0.05);
            let offset_len =
                (local_offset[0] * local_offset[0] + local_offset[1] * local_offset[1]).sqrt();
            let offset_scale = if offset_len > scale * 0.65 {
                scale * 0.65 / offset_len
            } else {
                1.0
            };
            fine[node] = [
                parent[0] + local_offset[0] * offset_scale + jitter[0],
                parent[1] + local_offset[1] * offset_scale + jitter[1],
            ];
        }
        positions = fine;
        relax(
            level,
            &mut positions,
            iterations_for(level.node_count, false),
        );
    }

    center(&mut positions);
    positions
}

fn coarsen(level: &Level, positions: &[Pos2]) -> (Level, Vec<u32>, Vec<Pos2>) {
    let mut map = vec![u32::MAX; level.node_count];
    let mut coarse_count = 0u32;

    // Deterministic greedy edge matching. Long chains therefore contract close
    // to 2:1, while branch hubs do not absorb arbitrary numbers of neighbours.
    for edge in &level.edges {
        let a = edge.a as usize;
        let b = edge.b as usize;
        if map[a] == u32::MAX && map[b] == u32::MAX {
            map[a] = coarse_count;
            map[b] = coarse_count;
            coarse_count += 1;
        }
    }
    for slot in &mut map {
        if *slot == u32::MAX {
            *slot = coarse_count;
            coarse_count += 1;
        }
    }

    let mut masses = vec![0.0f32; coarse_count as usize];
    let mut coarse_positions = vec![[0.0f32; 2]; coarse_count as usize];
    for (fine, &coarse) in map.iter().enumerate() {
        let mass = level.masses[fine];
        masses[coarse as usize] += mass;
        coarse_positions[coarse as usize][0] += positions[fine][0] * mass;
        coarse_positions[coarse as usize][1] += positions[fine][1] * mass;
    }
    for (position, &mass) in coarse_positions.iter_mut().zip(&masses) {
        let inv = 1.0 / mass.max(EPSILON);
        position[0] *= inv;
        position[1] *= inv;
    }

    let mut aggregated: AHashMap<(u32, u32), (f32, f32)> = AHashMap::new();
    for edge in &level.edges {
        let mut a = map[edge.a as usize];
        let mut b = map[edge.b as usize];
        if a == b {
            continue;
        }
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        let entry = aggregated.entry((a, b)).or_insert((0.0, 0.0));
        entry.0 += edge.desired * edge.weight;
        entry.1 += edge.weight;
    }

    let mut edges: Vec<Edge> = aggregated
        .into_iter()
        .map(|((a, b), (weighted_length, weight))| Edge {
            a,
            b,
            desired: (weighted_length / weight.max(EPSILON)).max(1.0),
            weight,
        })
        .collect();
    edges.sort_unstable_by_key(|edge| (edge.a, edge.b));

    (
        Level {
            node_count: coarse_count as usize,
            edges,
            masses,
            to_coarse: None,
        },
        map,
        coarse_positions,
    )
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn unit_from_hash(value: u64) -> f32 {
    ((value >> 40) as u32) as f32 / 16_777_215.0
}

fn deterministic_coarse_seed(node_count: usize, natural: f32) -> Vec<Pos2> {
    let side = natural * (node_count as f32).sqrt().max(2.0) * 0.9;
    let half = side * 0.5;
    (0..node_count)
        .map(|index| {
            let base = splitmix64(index as u64 ^ 0xA24B_AED4_963E_E407);
            let other = splitmix64(base ^ 0x9FB2_1C65_1E98_DF25);
            [
                (unit_from_hash(base) * 2.0 - 1.0) * half,
                (unit_from_hash(other) * 2.0 - 1.0) * half,
            ]
        })
        .collect()
}

fn deterministic_jitter(index: usize, radius: f32) -> Pos2 {
    let mixed = (index as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        ^ 0xD1B5_4A32_D192_ED03;
    let unit = (mixed as u32) as f32 / u32::MAX as f32;
    let angle = unit * std::f32::consts::TAU;
    let radial =
        radius * (0.35 + 0.65 * (((mixed >> 32) as u32) as f32 / u32::MAX as f32));
    [angle.cos() * radial, angle.sin() * radial]
}

fn mean_edge_length(level: &Level) -> f32 {
    if level.edges.is_empty() {
        return 100.0;
    }
    let (weighted_sum, total_weight) = level.edges.iter().fold((0.0, 0.0), |acc, edge| {
        (acc.0 + edge.desired * edge.weight, acc.1 + edge.weight)
    });
    weighted_sum / total_weight.max(1.0)
}

fn iterations_for(node_count: usize, coarsest: bool) -> usize {
    if coarsest {
        36
    } else if node_count <= 2_000 {
        20
    } else if node_count <= 20_000 {
        12
    } else if node_count <= 100_000 {
        7
    } else {
        4
    }
}

fn relax(level: &Level, positions: &mut [Pos2], iterations: usize) {
    if positions.len() <= 1 || iterations == 0 {
        return;
    }
    let natural = mean_edge_length(level).max(10.0);
    let repulsion_scale = natural * natural;
    let mut forces = vec![[0.0f32; 2]; positions.len()];

    for iteration in 0..iterations {
        let tree = BarnesHutTree::build(positions, &level.masses);
        forces
            .par_iter_mut()
            .enumerate()
            .for_each(|(node, force)| {
                *force =
                    tree.repulsion(node, positions, &level.masses, repulsion_scale, THETA);
            });

        for edge in &level.edges {
            let a = edge.a as usize;
            let b = edge.b as usize;
            let dx = positions[b][0] - positions[a][0];
            let dy = positions[b][1] - positions[a][1];
            let dist = (dx * dx + dy * dy).sqrt().max(EPSILON);
            let desired = edge.desired.max(1.0);
            let ratio = (dist / desired).max(EPSILON);

            // Match the shape of Bandage/OGDF's fmNew attraction:
            // log2(d/l) * d^2/l^3, then scaled by the graph's mean
            // ideal edge length squared. This becomes much stronger than
            // repulsion for badly stretched edges, preventing annular blow-up.
            let attraction =
                natural * natural * ratio.log2() * dist * dist / (desired * desired * desired);
            let magnitude = attraction * edge.weight.sqrt();
            let fx = dx / dist * magnitude;
            let fy = dy / dist * magnitude;
            forces[a][0] += fx;
            forces[a][1] += fy;
            forces[b][0] -= fx;
            forces[b][1] -= fy;
        }

        let progress = iteration as f32 / iterations.max(1) as f32;
        let temperature = natural * (0.18 * (-3.5 * progress).exp() + 0.008);
        positions
            .par_iter_mut()
            .zip(forces.par_iter())
            .zip(level.masses.par_iter())
            .for_each(|((position, force), &mass)| {
                let dx = force[0] / mass.max(1.0);
                let dy = force[1] / mass.max(1.0);
                let movement = (dx * dx + dy * dy).sqrt();
                if movement > EPSILON {
                    let step = movement.min(temperature) / movement;
                    position[0] += dx * step;
                    position[1] += dy * step;
                }
            });

        if iteration % 4 == 3 || iteration + 1 == iterations {
            center(positions);
        }
    }
}

fn center(positions: &mut [Pos2]) {
    if positions.is_empty() {
        return;
    }
    let mut center = [0.0f64; 2];
    for point in positions.iter() {
        center[0] += point[0] as f64;
        center[1] += point[1] as f64;
    }
    center[0] /= positions.len() as f64;
    center[1] /= positions.len() as f64;
    positions.par_iter_mut().for_each(|point| {
        point[0] -= center[0] as f32;
        point[1] -= center[1] as f32;
    });
}

#[derive(Clone, Copy)]
struct QuadNode {
    center: Pos2,
    half: f32,
    mass: f32,
    com: Pos2,
    point: i32,
    children: [i32; 4],
}

impl QuadNode {
    fn empty(center: Pos2, half: f32) -> Self {
        Self {
            center,
            half,
            mass: 0.0,
            com: [0.0, 0.0],
            point: -1,
            children: [-1; 4],
        }
    }

    fn is_leaf(&self) -> bool {
        self.children[0] < 0
    }

    fn contains(&self, point: Pos2) -> bool {
        (point[0] - self.center[0]).abs() <= self.half
            && (point[1] - self.center[1]).abs() <= self.half
    }
}

struct BarnesHutTree {
    nodes: Vec<QuadNode>,
}

impl BarnesHutTree {
    fn build(positions: &[Pos2], masses: &[f32]) -> Self {
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for point in positions {
            min[0] = min[0].min(point[0]);
            min[1] = min[1].min(point[1]);
            max[0] = max[0].max(point[0]);
            max[1] = max[1].max(point[1]);
        }
        let center = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
        let half = ((max[0] - min[0]).max(max[1] - min[1]) * 0.5)
            .max(1.0)
            * 1.0001;
        let mut tree = Self {
            nodes: vec![QuadNode::empty(center, half)],
        };
        for point in 0..positions.len() {
            tree.insert(0, point, positions, masses, 0);
        }
        tree
    }

    fn insert(
        &mut self,
        node_index: usize,
        point_index: usize,
        positions: &[Pos2],
        masses: &[f32],
        depth: usize,
    ) {
        let point = positions[point_index];
        let mass = masses[point_index].max(EPSILON);

        {
            let node = &mut self.nodes[node_index];
            let new_mass = node.mass + mass;
            node.com[0] = (node.com[0] * node.mass + point[0] * mass) / new_mass;
            node.com[1] = (node.com[1] * node.mass + point[1] * mass) / new_mass;
            node.mass = new_mass;
        }

        let is_leaf = self.nodes[node_index].is_leaf();
        let existing = self.nodes[node_index].point;

        if is_leaf && existing == -1 {
            self.nodes[node_index].point = point_index as i32;
            return;
        }

        if is_leaf && (depth >= 28 || self.nodes[node_index].half <= EPSILON) {
            // Degenerate coincident points remain an aggregate leaf. Querying
            // subtracts the target's own contribution when necessary.
            self.nodes[node_index].point = -2;
            return;
        }

        if is_leaf {
            self.subdivide(node_index);
            self.nodes[node_index].point = -2;
            if existing >= 0 {
                let old = existing as usize;
                let child = self.child_index(node_index, positions[old]);
                self.insert(child, old, positions, masses, depth + 1);
            }
        }

        let child = self.child_index(node_index, point);
        self.insert(child, point_index, positions, masses, depth + 1);
    }

    fn subdivide(&mut self, node_index: usize) {
        let node = self.nodes[node_index];
        let child_half = node.half * 0.5;
        let first = self.nodes.len();
        for quadrant in 0..4 {
            let x = if quadrant & 1 == 0 { -1.0 } else { 1.0 };
            let y = if quadrant & 2 == 0 { -1.0 } else { 1.0 };
            self.nodes.push(QuadNode::empty(
                [
                    node.center[0] + x * child_half,
                    node.center[1] + y * child_half,
                ],
                child_half,
            ));
        }
        self.nodes[node_index].children = [
            first as i32,
            (first + 1) as i32,
            (first + 2) as i32,
            (first + 3) as i32,
        ];
    }

    fn child_index(&self, node_index: usize, point: Pos2) -> usize {
        let node = self.nodes[node_index];
        let x = usize::from(point[0] >= node.center[0]);
        let y = usize::from(point[1] >= node.center[1]) * 2;
        node.children[x + y] as usize
    }

    fn repulsion(
        &self,
        target: usize,
        positions: &[Pos2],
        masses: &[f32],
        scale: f32,
        theta: f32,
    ) -> Pos2 {
        self.repulsion_from(0, target, positions, masses, scale, theta)
    }

    fn repulsion_from(
        &self,
        node_index: usize,
        target: usize,
        positions: &[Pos2],
        masses: &[f32],
        scale: f32,
        theta: f32,
    ) -> Pos2 {
        let node = self.nodes[node_index];
        if node.mass <= EPSILON {
            return [0.0, 0.0];
        }

        let target_point = positions[target];

        if node.is_leaf() {
            if node.point == target as i32 {
                return [0.0, 0.0];
            }
            if node.point >= 0 {
                return repulsive_force(target_point, node.com, node.mass, scale);
            }

            let mut mass = node.mass;
            let mut com = node.com;
            if node.contains(target_point) {
                let target_mass = masses[target].max(EPSILON);
                let remaining = mass - target_mass;
                if remaining <= EPSILON {
                    return [0.0, 0.0];
                }
                com = [
                    (com[0] * mass - target_point[0] * target_mass) / remaining,
                    (com[1] * mass - target_point[1] * target_mass) / remaining,
                ];
                mass = remaining;
            }
            return repulsive_force(target_point, com, mass, scale);
        }

        let dx = target_point[0] - node.com[0];
        let dy = target_point[1] - node.com[1];
        let dist2 = (dx * dx + dy * dy).max(EPSILON);
        let width = node.half * 2.0;
        if !node.contains(target_point) && width * width < theta * theta * dist2 {
            return repulsive_force(target_point, node.com, node.mass, scale);
        }

        let mut force = [0.0, 0.0];
        for child in node.children {
            if child >= 0 {
                let child_force =
                    self.repulsion_from(child as usize, target, positions, masses, scale, theta);
                force[0] += child_force[0];
                force[1] += child_force[1];
            }
        }
        force
    }
}

fn repulsive_force(target: Pos2, source: Pos2, source_mass: f32, scale: f32) -> Pos2 {
    let dx = target[0] - source[0];
    let dy = target[1] - source[1];
    let dist2 = (dx * dx + dy * dy).max(1.0);
    [
        scale * source_mass * dx / dist2,
        scale * source_mass * dy / dist2,
    ]
}

struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
            rank: vec![0; size],
        }
    }

    fn find(&mut self, node: usize) -> usize {
        let mut root = node;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut current = node;
        while self.parent[current] != current {
            let next = self.parent[current];
            self.parent[current] = root;
            current = next;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node_count: usize, edges: &[(u32, u32)]) -> Vec<Pos2> {
        let from: Vec<_> = edges.iter().map(|edge| edge.0).collect();
        let to: Vec<_> = edges.iter().map(|edge| edge.1).collect();
        let lengths = vec![100.0; edges.len()];
        let initial: Vec<Pos2> = (0..node_count)
            .map(|index| [index as f32 * 100.0, 0.0])
            .collect();
        let mut output = vec![[0.0; 2]; node_count];
        initial_layout(node_count, &from, &to, &lengths, &initial, &mut output).unwrap();
        output
    }

    #[test]
    fn path_is_finite_and_deterministic() {
        let edges: Vec<_> = (1..200).map(|node| (node - 1, node)).collect();
        let first = run(200, &edges);
        let second = run(200, &edges);
        assert_eq!(first, second);
        assert!(first.iter().flatten().all(|value| value.is_finite()));
    }

    #[test]
    fn branched_graph_spreads_nodes() {
        let mut edges: Vec<(u32, u32)> =
            (1..1000).map(|node| (node - 1, node)).collect();
        edges.extend((0..997).step_by(17).map(|node| (node, node + 3)));
        let output = run(1000, &edges);
        let min_x = output
            .iter()
            .map(|point| point[0])
            .fold(f32::INFINITY, f32::min);
        let max_x = output
            .iter()
            .map(|point| point[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = output
            .iter()
            .map(|point| point[1])
            .fold(f32::INFINITY, f32::min);
        let max_y = output
            .iter()
            .map(|point| point[1])
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(max_x - min_x > 10.0);
        assert!(max_y - min_y > 10.0);
    }

    #[test]
    fn disconnected_components_are_solved_independently() {
        let output = run(6, &[(0, 1), (1, 2), (3, 4), (4, 5)]);
        assert!(output.iter().flatten().all(|value| value.is_finite()));
    }
}
