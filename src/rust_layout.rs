use ahash::AHashMap;
use rayon::prelude::*;
use std::collections::VecDeque;

pub type Pos2 = [f32; 2];

const COARSE_TARGET: usize = 50;
const EXACT_REPULSION_LIMIT: usize = 175;
const MAX_LEVELS: usize = 30;
const THETA: f32 = 0.72;
const EPSILON: f32 = 1.0e-5;
const FORCE_SCALING: f32 = 0.05;
const WAGGLE_FACTOR: f32 = 0.05;

#[derive(Clone, Copy, Debug)]
struct Edge {
    a: u32,
    b: u32,
    desired: f32,
}

#[derive(Clone)]
struct Prolongation {
    /// Fine node -> coarse solar-system node.
    parent: Vec<u32>,
    /// True for the fine node chosen as the sun/representative.
    is_sun: Vec<bool>,
    /// Metric distance from a fine node to its dedicated sun.
    distance_to_sun: Vec<f32>,
    /// CSR offsets into neighbour_parent / lambda.
    constraint_offsets: Vec<usize>,
    /// Adjacent coarse sun for each inter-solar-system constraint.
    neighbour_parent: Vec<u32>,
    /// Fraction from own sun toward neighbour sun.
    lambda: Vec<f32>,
}

#[derive(Clone)]
struct Level {
    node_count: usize,
    edges: Vec<Edge>,
    /// Used only to choose low-mass solar-system representatives.
    hierarchy_mass: Vec<u32>,
    /// Present on a fine level when a coarser level exists.
    prolongation: Option<Prolongation>,
}

struct ComponentWork {
    global_nodes: Vec<usize>,
    edges: Vec<Edge>,
}

struct ComponentResult {
    global_nodes: Vec<usize>,
    positions: Vec<Pos2>,
}

#[derive(Clone)]
struct Adjacency {
    offsets: Vec<usize>,
    neighbours: Vec<u32>,
    edge_indices: Vec<usize>,
}

impl Adjacency {
    fn build(node_count: usize, edges: &[Edge]) -> Self {
        let mut degree = vec![0usize; node_count];
        for edge in edges {
            degree[edge.a as usize] += 1;
            degree[edge.b as usize] += 1;
        }
        let mut offsets = vec![0usize; node_count + 1];
        for i in 0..node_count {
            offsets[i + 1] = offsets[i] + degree[i];
        }
        let mut cursor = offsets[..node_count].to_vec();
        let mut neighbours = vec![0u32; offsets[node_count]];
        let mut edge_indices = vec![0usize; offsets[node_count]];
        for (edge_index, edge) in edges.iter().enumerate() {
            let a = edge.a as usize;
            let b = edge.b as usize;
            let ia = cursor[a];
            neighbours[ia] = edge.b;
            edge_indices[ia] = edge_index;
            cursor[a] += 1;

            let ib = cursor[b];
            neighbours[ib] = edge.a;
            edge_indices[ib] = edge_index;
            cursor[b] += 1;
        }
        Self {
            offsets,
            neighbours,
            edge_indices,
        }
    }

    fn range(&self, node: usize) -> std::ops::Range<usize> {
        self.offsets[node]..self.offsets[node + 1]
    }

    fn degree(&self, node: usize) -> usize {
        self.offsets[node + 1] - self.offsets[node]
    }
}

/// Graphite-specific Rust implementation of the important FM^3 ideas used by
/// Bandage/OGDF. Long-range repulsion is Barnes-Hut rather than OGDF's NMM,
/// while the multilevel hierarchy follows the solar-system strategy.
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

    // FMMM starts from a simple loop-free graph. Parallel/reversed edges are
    // collapsed and their ideal lengths are averaged.
    let edges = simplify_edges(node_count, from, to, lengths)?;
    let components = split_components(node_count, &edges);

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

fn simplify_edges(
    node_count: usize,
    from: &[u32],
    to: &[u32],
    lengths: &[f32],
) -> Result<Vec<Edge>, &'static str> {
    let mut merged: AHashMap<(u32, u32), (f64, u32)> = AHashMap::new();
    for ((&a, &b), &desired) in from.iter().zip(to).zip(lengths) {
        if a as usize >= node_count || b as usize >= node_count {
            return Err("layout edge endpoint is out of range");
        }
        if a == b {
            continue;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        let entry = merged.entry(key).or_insert((0.0, 0));
        entry.0 += desired.max(1.0) as f64;
        entry.1 += 1;
    }

    let mut edges: Vec<Edge> = merged
        .into_iter()
        .map(|((a, b), (sum, count))| Edge {
            a,
            b,
            desired: (sum / count.max(1) as f64) as f32,
        })
        .collect();
    edges.sort_unstable_by_key(|edge| (edge.a, edge.b));
    Ok(edges)
}

fn split_components(node_count: usize, edges: &[Edge]) -> Vec<ComponentWork> {
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
        });
    }

    global_nodes
        .into_iter()
        .zip(component_edges)
        .map(|(global_nodes, edges)| ComponentWork {
            global_nodes,
            edges,
        })
        .collect()
}

fn solve_component(work: ComponentWork) -> ComponentResult {
    let node_count = work.global_nodes.len();
    // Keep runs reproducible while ensuring isomorphic components do not all
    // start from the exact same local random state.
    let component_seed = work
        .global_nodes
        .iter()
        .fold(0xA076_1D64_78BD_642Fu64, |seed, &node| {
            splitmix64(seed ^ (node as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        });
    let positions = match node_count {
        0 => Vec::new(),
        1 => vec![[0.0, 0.0]],
        2 => {
            let length = work.edges.first().map_or(100.0, |edge| edge.desired);
            vec![[-0.5 * length, 0.0], [0.5 * length, 0.0]]
        }
        _ => solve_multilevel(node_count, work.edges, component_seed),
    };

    ComponentResult {
        global_nodes: work.global_nodes,
        positions,
    }
}

fn solve_multilevel(node_count: usize, edges: Vec<Edge>, component_seed: u64) -> Vec<Pos2> {
    let mut levels = vec![Level {
        node_count,
        edges,
        hierarchy_mass: vec![1; node_count],
        prolongation: None,
    }];

    while levels.last().is_some_and(|level| level.node_count > COARSE_TARGET)
        && levels.len() < MAX_LEVELS
    {
        let level_index = levels.len() - 1;
        let (coarse, prolongation) =
            solar_coarsen(&levels[level_index], component_seed ^ level_index as u64);
        if coarse.node_count >= levels[level_index].node_count {
            break;
        }
        levels[level_index].prolongation = Some(prolongation);
        levels.push(coarse);
    }

    let max_level = levels.len() - 1;
    let base_iterations = if node_count > 50_000 { 3 } else { 12 };
    let fine_tuning_iterations = if node_count > 50_000 { 1 } else { 8 };

    let coarsest = &levels[max_level];
    let natural = mean_edge_length(coarsest).max(5.0);
    let mut positions =
        deterministic_random_seed(coarsest.node_count, natural, component_seed);
    let iterations = multilevel_iterations(
        max_level,
        max_level,
        coarsest.node_count,
        base_iterations,
    );
    run_force_iterations(coarsest, &mut positions, iterations, ForcePhase::Normal);

    for level_index in (0..max_level).rev() {
        let level = &levels[level_index];
        let prolongation = level
            .prolongation
            .as_ref()
            .expect("fine level must contain prolongation metadata");
        positions = prolong_positions(
            level,
            prolongation,
            &positions,
            component_seed ^ (level_index as u64).rotate_left(17),
        );
        let iterations = multilevel_iterations(
            level_index,
            max_level,
            level.node_count,
            base_iterations,
        );
        run_force_iterations(level, &mut positions, iterations, ForcePhase::Normal);
    }

    // Bandage adds this assembly-graph-specific cleanup before FMMM's normal
    // postprocessing. It untwists simple equal-length split/merge bubbles.
    fix_twisted_splits(&levels[0], &mut positions);

    // FMMM postprocessing: ten cooler normal-force iterations, rescale to the
    // requested average ideal edge length, then low-repulsion/high-spring fine
    // tuning and a final rescale.
    run_force_iterations(&levels[0], &mut positions, 10, ForcePhase::Post);
    rescale_to_ideal_edge_length(&levels[0], &mut positions);

    if fine_tuning_iterations > 0 {
        run_force_iterations(
            &levels[0],
            &mut positions,
            fine_tuning_iterations,
            ForcePhase::Fine {
                total: fine_tuning_iterations,
            },
        );
        rescale_to_ideal_edge_length(&levels[0], &mut positions);
    }

    center(&mut positions);
    positions
}

fn solar_coarsen(level: &Level, salt: u64) -> (Level, Prolongation) {
    let adjacency = Adjacency::build(level.node_count, &level.edges);

    // OGDF's gcNonUniformProbLowerMass favours low star-mass nodes as suns.
    // We make the selection deterministic by sorting on star mass and a hash.
    let mut candidates: Vec<usize> = (0..level.node_count).collect();
    let star_mass: Vec<u64> = (0..level.node_count)
        .map(|node| {
            let mut mass = level.hierarchy_mass[node] as u64;
            for index in adjacency.range(node) {
                mass += level.hierarchy_mass[adjacency.neighbours[index] as usize] as u64;
            }
            mass
        })
        .collect();
    candidates.sort_unstable_by_key(|&node| {
        (
            star_mass[node],
            splitmix64(node as u64 ^ salt ^ 0x31D0_8C59_EA22_4A9B),
        )
    });

    let mut blocked = vec![false; level.node_count];
    let mut suns = Vec::new();
    for node in candidates {
        if blocked[node] {
            continue;
        }
        suns.push(node);
        blocked[node] = true;

        // Remove planets and possible moons from the future-sun candidate set,
        // matching OGDF's solar-system partitioning.
        for index in adjacency.range(node) {
            let planet = adjacency.neighbours[index] as usize;
            blocked[planet] = true;
            for neighbour_index in adjacency.range(planet) {
                blocked[adjacency.neighbours[neighbour_index] as usize] = true;
            }
        }
    }
    if suns.is_empty() {
        suns.push(0);
    }

    let coarse_count = suns.len();
    let mut parent = vec![u32::MAX; level.node_count];
    let mut is_sun = vec![false; level.node_count];
    let mut distance_to_sun = vec![f32::INFINITY; level.node_count];

    for (coarse, &sun) in suns.iter().enumerate() {
        parent[sun] = coarse as u32;
        is_sun[sun] = true;
        distance_to_sun[sun] = 0.0;
    }

    // Direct neighbours of suns are planets. If a node borders multiple suns,
    // retain the shortest dedicated-sun edge.
    for (coarse, &sun) in suns.iter().enumerate() {
        for index in adjacency.range(sun) {
            let node = adjacency.neighbours[index] as usize;
            if is_sun[node] {
                continue;
            }
            let edge = level.edges[adjacency.edge_indices[index]];
            if edge.desired < distance_to_sun[node] {
                parent[node] = coarse as u32;
                distance_to_sun[node] = edge.desired;
            }
        }
    }

    // Remaining moon nodes inherit a solar system through an already assigned
    // neighbour. The solar-system construction guarantees short paths; this
    // queue also makes the code robust to unusual sparse structures.
    let mut queue = VecDeque::new();
    for node in 0..level.node_count {
        if parent[node] != u32::MAX {
            queue.push_back(node);
        }
    }
    while let Some(node) = queue.pop_front() {
        let parent_node = parent[node];
        let base_distance = distance_to_sun[node];
        for index in adjacency.range(node) {
            let neighbour = adjacency.neighbours[index] as usize;
            if parent[neighbour] != u32::MAX {
                continue;
            }
            let edge = level.edges[adjacency.edge_indices[index]];
            parent[neighbour] = parent_node;
            distance_to_sun[neighbour] = base_distance + edge.desired;
            queue.push_back(neighbour);
        }
    }
    for node in 0..level.node_count {
        if parent[node] == u32::MAX {
            parent[node] = 0;
            distance_to_sun[node] = 0.0;
        }
    }

    // OGDF mass is hierarchy metadata, not a physical charge. At the new level
    // it is the number of lower-level graph nodes represented by each sun.
    let mut hierarchy_mass = vec![0u32; coarse_count];
    for &coarse in &parent {
        hierarchy_mass[coarse as usize] = hierarchy_mass[coarse as usize].saturating_add(1);
    }

    // Count inter-solar constraints first so the fine-level metadata can be
    // stored compactly as CSR rather than one Vec per node.
    let mut constraint_count = vec![0usize; level.node_count];
    let mut merged: AHashMap<(u32, u32), (f64, u32)> = AHashMap::new();
    for edge in &level.edges {
        let a = edge.a as usize;
        let b = edge.b as usize;
        let pa = parent[a];
        let pb = parent[b];
        if pa == pb {
            continue;
        }
        let new_length = (distance_to_sun[a] + edge.desired + distance_to_sun[b]).max(1.0);
        let key = if pa < pb { (pa, pb) } else { (pb, pa) };
        let entry = merged.entry(key).or_insert((0.0, 0));
        entry.0 += new_length as f64;
        entry.1 += 1;
        constraint_count[a] += 1;
        constraint_count[b] += 1;
    }

    let mut constraint_offsets = vec![0usize; level.node_count + 1];
    for node in 0..level.node_count {
        constraint_offsets[node + 1] = constraint_offsets[node] + constraint_count[node];
    }
    let mut cursor = constraint_offsets[..level.node_count].to_vec();
    let mut neighbour_parent = vec![0u32; constraint_offsets[level.node_count]];
    let mut lambda = vec![0.0f32; constraint_offsets[level.node_count]];

    for edge in &level.edges {
        let a = edge.a as usize;
        let b = edge.b as usize;
        let pa = parent[a];
        let pb = parent[b];
        if pa == pb {
            continue;
        }
        let new_length = (distance_to_sun[a] + edge.desired + distance_to_sun[b]).max(1.0);

        let ia = cursor[a];
        neighbour_parent[ia] = pb;
        lambda[ia] = (distance_to_sun[a] / new_length).clamp(0.0, 1.0);
        cursor[a] += 1;

        let ib = cursor[b];
        neighbour_parent[ib] = pa;
        lambda[ib] = (distance_to_sun[b] / new_length).clamp(0.0, 1.0);
        cursor[b] += 1;
    }

    // Parallel coarse edges are reduced to one edge whose ideal length is the
    // average. Multiplicity is deliberately not converted into spring weight.
    let mut edges: Vec<Edge> = merged
        .into_iter()
        .map(|((a, b), (sum, count))| Edge {
            a,
            b,
            desired: (sum / count.max(1) as f64) as f32,
        })
        .collect();
    edges.sort_unstable_by_key(|edge| (edge.a, edge.b));

    (
        Level {
            node_count: coarse_count,
            edges,
            hierarchy_mass,
            prolongation: None,
        },
        Prolongation {
            parent,
            is_sun,
            distance_to_sun,
            constraint_offsets,
            neighbour_parent,
            lambda,
        },
    )
}

fn prolong_positions(
    level: &Level,
    prolongation: &Prolongation,
    coarse_positions: &[Pos2],
    salt: u64,
) -> Vec<Pos2> {
    let mut positions = vec![[0.0f32; 2]; level.node_count];

    for node in 0..level.node_count {
        let own_parent = prolongation.parent[node] as usize;
        let own_sun = coarse_positions[own_parent];
        if prolongation.is_sun[node] {
            positions[node] = own_sun;
            continue;
        }

        let start = prolongation.constraint_offsets[node];
        let end = prolongation.constraint_offsets[node + 1];

        if start < end {
            let mut sum = [0.0f32; 2];
            let mut count = 0.0f32;
            for constraint in start..end {
                let other = coarse_positions[prolongation.neighbour_parent[constraint] as usize];
                let lambda = prolongation.lambda[constraint];
                let base = [
                    own_sun[0] + lambda * (other[0] - own_sun[0]),
                    own_sun[1] + lambda * (other[1] - own_sun[1]),
                ];
                let span = distance(own_sun, other);
                let wiggle = deterministic_waggle(
                    node as u64
                        ^ (constraint as u64).rotate_left(19)
                        ^ salt.rotate_left(7),
                    span * WAGGLE_FACTOR,
                );
                sum[0] += base[0] + wiggle[0];
                sum[1] += base[1] + wiggle[1];
                count += 1.0;
            }
            positions[node] = [sum[0] / count, sum[1] / count];
        } else {
            let radius = prolongation.distance_to_sun[node].max(1.0);
            let offset = deterministic_radius_position(
                node as u64 ^ salt ^ 0xA24B_AED4_963E_E407,
                radius,
            );
            positions[node] = [own_sun[0] + offset[0], own_sun[1] + offset[1]];
        }
    }

    positions
}

fn multilevel_iterations(
    act_level: usize,
    max_level: usize,
    node_count: usize,
    fixed_iterations: usize,
) -> usize {
    let iterations = if max_level == 0 {
        fixed_iterations * 10
    } else {
        let ratio = act_level as f32 / max_level as f32;
        fixed_iterations
            + (ratio * (9 * fixed_iterations) as f32).round() as usize
    };
    if node_count <= 500 {
        iterations.max(100)
    } else {
        iterations
    }
}

#[derive(Clone, Copy)]
enum ForcePhase {
    Normal,
    Post,
    Fine { total: usize },
}

fn run_force_iterations(
    level: &Level,
    positions: &mut [Pos2],
    iterations: usize,
    phase: ForcePhase,
) {
    if positions.len() <= 1 || iterations == 0 {
        return;
    }

    let average_ideal = mean_edge_length(level).max(1.0);
    let mut attraction = vec![[0.0f32; 2]; positions.len()];
    let mut repulsion = vec![[0.0f32; 2]; positions.len()];
    let mut movement = vec![[0.0f32; 2]; positions.len()];
    let mut previous_movement = vec![[0.0f32; 2]; positions.len()];

    for iteration in 0..iterations {
        attraction.fill([0.0, 0.0]);

        for edge in &level.edges {
            let a = edge.a as usize;
            let b = edge.b as usize;
            let dx = positions[b][0] - positions[a][0];
            let dy = positions[b][1] - positions[a][1];
            let d = (dx * dx + dy * dy).sqrt();
            if d <= EPSILON {
                continue;
            }
            let ideal = edge.desired.max(1.0);
            let scalar = (d / ideal).max(EPSILON).log2() * d * d
                / (ideal * ideal * ideal);
            let fx = dx / d * scalar;
            let fy = dy / d * scalar;
            attraction[a][0] += fx;
            attraction[a][1] += fy;
            attraction[b][0] -= fx;
            attraction[b][1] -= fy;
        }

        if positions.len() < EXACT_REPULSION_LIMIT {
            repulsion
                .par_iter_mut()
                .enumerate()
                .for_each(|(target, force)| {
                    let mut total = [0.0f32; 2];
                    for source in 0..positions.len() {
                        if source == target {
                            continue;
                        }
                        let pair = repulsive_force(positions[target], positions[source], 1.0);
                        total[0] += pair[0];
                        total[1] += pair[1];
                    }
                    *force = total;
                });
        } else {
            let tree = BarnesHutTree::build(positions);
            repulsion
                .par_iter_mut()
                .enumerate()
                .for_each(|(target, force)| {
                    *force = tree.repulsion(target, positions, THETA);
                });
        }

        let (spring_strength, repulsion_strength, cool_factor) = match phase {
            ForcePhase::Normal => (1.0, 1.0, 1.0),
            ForcePhase::Post => (1.0, 1.0, 0.1),
            ForcePhase::Fine { total } => {
                let cool = if iteration + 1 <= total.saturating_sub(5) {
                    0.2
                } else {
                    0.02
                };
                (2.0, (400.0 / positions.len() as f32).min(0.2), cool)
            }
        };

        let scale = average_ideal * average_ideal;
        let box_length = current_box_length(positions);
        let max_radius = if iteration == 0 {
            box_length / 1000.0
        } else {
            box_length / 5.0
        }
        .max(0.01);

        movement
            .par_iter_mut()
            .enumerate()
            .for_each(|(node, out)| {
                let fx = scale
                    * (spring_strength * attraction[node][0]
                        + repulsion_strength * repulsion[node][0]);
                let fy = scale
                    * (spring_strength * attraction[node][1]
                        + repulsion_strength * repulsion[node][1]);
                let norm = (fx * fx + fy * fy).sqrt();
                if norm <= EPSILON {
                    *out = [0.0, 0.0];
                    return;
                }
                let allowed = (norm * cool_factor * FORCE_SCALING).min(max_radius);
                *out = [fx / norm * allowed, fy / norm * allowed];
            });

        if iteration > 0 {
            movement
                .par_iter_mut()
                .zip(previous_movement.par_iter())
                .for_each(|(new, old)| prevent_oscillation(new, *old));
        }

        let average_move = movement
            .par_iter()
            .map(|m| (m[0] * m[0] + m[1] * m[1]).sqrt())
            .sum::<f32>()
            / positions.len() as f32;

        positions
            .par_iter_mut()
            .zip(movement.par_iter())
            .for_each(|(position, delta)| {
                position[0] += delta[0];
                position[1] += delta[1];
            });

        previous_movement.clone_from_slice(&movement);

        if iteration % 8 == 7 {
            center(positions);
        }
        if iteration >= 4 && average_move < 0.01 {
            break;
        }
    }
}

fn prevent_oscillation(new: &mut Pos2, old: Pos2) {
    let new_norm = (new[0] * new[0] + new[1] * new[1]).sqrt();
    let old_norm = (old[0] * old[0] + old[1] * old[1]).sqrt();
    if new_norm <= EPSILON || old_norm <= EPSILON {
        return;
    }

    let cos_angle =
        ((new[0] * old[0] + new[1] * old[1]) / (new_norm * old_norm)).clamp(-1.0, 1.0);
    let angle = cos_angle.acos();

    let max_factor = if angle <= std::f32::consts::FRAC_PI_6 {
        2.0
    } else if angle <= std::f32::consts::FRAC_PI_3 {
        1.5
    } else if angle <= std::f32::consts::FRAC_PI_2 {
        1.0
    } else if angle <= 2.0 * std::f32::consts::FRAC_PI_3 {
        2.0 / 3.0
    } else if angle <= 5.0 * std::f32::consts::FRAC_PI_6 {
        0.5
    } else {
        1.0 / 3.0
    };

    let max_norm = old_norm * max_factor;
    if new_norm > max_norm {
        let scale = max_norm / new_norm;
        new[0] *= scale;
        new[1] *= scale;
    }
}

fn current_box_length(positions: &[Pos2]) -> f32 {
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    for point in positions {
        min[0] = min[0].min(point[0]);
        min[1] = min[1].min(point[1]);
        max[0] = max[0].max(point[0]);
        max[1] = max[1].max(point[1]);
    }
    let span = (max[0] - min[0]).max(max[1] - min[1]);
    if span <= 0.0 {
        positions.len() as f32 * 20.0
    } else {
        span * 1.01 + 2.0
    }
}

fn rescale_to_ideal_edge_length(level: &Level, positions: &mut [Pos2]) {
    if level.edges.is_empty() {
        return;
    }
    let mut ideal = 0.0f64;
    let mut actual = 0.0f64;
    for edge in &level.edges {
        ideal += edge.desired as f64;
        actual += distance(
            positions[edge.a as usize],
            positions[edge.b as usize],
        ) as f64;
    }
    if actual <= f64::EPSILON {
        return;
    }
    let factor = (ideal / actual) as f32;
    positions.par_iter_mut().for_each(|position| {
        position[0] *= factor;
        position[1] *= factor;
    });
}

fn mean_edge_length(level: &Level) -> f32 {
    if level.edges.is_empty() {
        return 50.0;
    }
    level.edges.iter().map(|edge| edge.desired).sum::<f32>() / level.edges.len() as f32
}

fn deterministic_random_seed(node_count: usize, natural: f32, salt: u64) -> Vec<Pos2> {
    // OGDF's zero-sized nodes give an initial box of roughly 11*n. That is
    // excessive for very large coarse edge lengths, so retain the same random
    // square idea while scaling it to graph size and ideal edge length.
    let side = (natural * (node_count as f32).sqrt() * 2.0)
        .max(natural * 4.0)
        .max(20.0);
    (0..node_count)
        .map(|node| {
            let x = unit_from_hash(splitmix64(
                node as u64 ^ salt ^ 0x69D5_7FC8_A2E4_7301,
            ));
            let y = unit_from_hash(splitmix64(
                node as u64 ^ salt.rotate_left(29) ^ 0xD2B7_4407_B1CE_6E93,
            ));
            [(x - 0.5) * side, (y - 0.5) * side]
        })
        .collect()
}

fn deterministic_radius_position(seed: u64, radius: f32) -> Pos2 {
    let angle = unit_from_hash(splitmix64(seed)) * std::f32::consts::TAU;
    [angle.cos() * radius, angle.sin() * radius]
}

fn deterministic_waggle(seed: u64, max_radius: f32) -> Pos2 {
    let h1 = splitmix64(seed);
    let h2 = splitmix64(h1 ^ 0x9FB2_1C65_1E98_DF25);
    let radius = unit_from_hash(h1) * max_radius;
    let angle = unit_from_hash(h2) * std::f32::consts::TAU;
    [angle.cos() * radius, angle.sin() * radius]
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

fn distance(a: Pos2, b: Pos2) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
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

// ── Bandage split untangling ─────────────────────────────────────────────────

fn fix_twisted_splits(level: &Level, positions: &mut [Pos2]) {
    let adjacency = Adjacency::build(level.node_count, &level.edges);

    for start in 0..level.node_count {
        if adjacency.degree(start) != 3 {
            continue;
        }
        let neighbours: Vec<usize> = adjacency
            .range(start)
            .map(|index| adjacency.neighbours[index] as usize)
            .collect();

        let paths: Vec<(usize, Vec<usize>)> = neighbours
            .iter()
            .map(|&first| follow_until_branch(start, first, &adjacency))
            .collect();

        let mut pair = None;
        for i in 0..3 {
            for j in i + 1..3 {
                let k = 3usize - i - j;
                if paths[i].0 == paths[j].0
                    && paths[i].1.len() == paths[j].1.len()
                    && paths[i].0 != paths[k].0
                    && paths[i].1.len() > 1
                {
                    pair = Some((i, j));
                    break;
                }
            }
            if pair.is_some() {
                break;
            }
        }

        let Some((a_index, b_index)) = pair else {
            continue;
        };
        let path_a = &paths[a_index].1;
        let path_b = &paths[b_index].1;

        for i in 0..path_a.len() - 1 {
            let a1 = path_a[i];
            let a2 = path_a[i + 1];
            let b1 = path_b[i];
            let b2 = path_b[i + 1];
            if segments_cross(
                positions[a1],
                positions[a2],
                positions[b1],
                positions[b2],
            ) {
                positions.swap(a2, b2);
            }
        }
    }
}

fn follow_until_branch(previous: usize, first: usize, adjacency: &Adjacency) -> (usize, Vec<usize>) {
    let mut prev = previous;
    let mut current = first;
    let mut path = vec![first];

    while adjacency.degree(current) == 2 && path.len() < adjacency.offsets.len() {
        let mut next = None;
        for index in adjacency.range(current) {
            let candidate = adjacency.neighbours[index] as usize;
            if candidate != prev {
                next = Some(candidate);
                break;
            }
        }
        let Some(candidate) = next else {
            break;
        };
        prev = current;
        current = candidate;
        if adjacency.degree(current) == 2 {
            path.push(current);
        }
    }

    (current, path)
}

fn segments_cross(a: Pos2, b: Pos2, c: Pos2, d: Pos2) -> bool {
    fn orient(a: Pos2, b: Pos2, c: Pos2) -> f32 {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    }
    let ab_c = orient(a, b, c);
    let ab_d = orient(a, b, d);
    let cd_a = orient(c, d, a);
    let cd_b = orient(c, d, b);
    ab_c * ab_d < 0.0 && cd_a * cd_b < 0.0
}

// ── Barnes-Hut repulsion ─────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct QuadNode {
    center: Pos2,
    half: f32,
    charge: f32,
    center_of_charge: Pos2,
    point: i32,
    children: [i32; 4],
}

impl QuadNode {
    fn empty(center: Pos2, half: f32) -> Self {
        Self {
            center,
            half,
            charge: 0.0,
            center_of_charge: [0.0, 0.0],
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
    fn build(positions: &[Pos2]) -> Self {
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
            tree.insert(0, point, positions, 0);
        }
        tree
    }

    fn insert(&mut self, node_index: usize, point_index: usize, positions: &[Pos2], depth: usize) {
        let point = positions[point_index];

        {
            let node = &mut self.nodes[node_index];
            let new_charge = node.charge + 1.0;
            node.center_of_charge[0] =
                (node.center_of_charge[0] * node.charge + point[0]) / new_charge;
            node.center_of_charge[1] =
                (node.center_of_charge[1] * node.charge + point[1]) / new_charge;
            node.charge = new_charge;
        }

        let is_leaf = self.nodes[node_index].is_leaf();
        let existing = self.nodes[node_index].point;

        if is_leaf && existing == -1 {
            self.nodes[node_index].point = point_index as i32;
            return;
        }

        if is_leaf && (depth >= 28 || self.nodes[node_index].half <= EPSILON) {
            self.nodes[node_index].point = -2;
            return;
        }

        if is_leaf {
            self.subdivide(node_index);
            self.nodes[node_index].point = -2;
            if existing >= 0 {
                let old = existing as usize;
                let child = self.child_index(node_index, positions[old]);
                self.insert(child, old, positions, depth + 1);
            }
        }

        let child = self.child_index(node_index, point);
        self.insert(child, point_index, positions, depth + 1);
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

    fn repulsion(&self, target: usize, positions: &[Pos2], theta: f32) -> Pos2 {
        self.repulsion_from(0, target, positions, theta)
    }

    fn repulsion_from(
        &self,
        node_index: usize,
        target: usize,
        positions: &[Pos2],
        theta: f32,
    ) -> Pos2 {
        let node = self.nodes[node_index];
        if node.charge <= EPSILON {
            return [0.0, 0.0];
        }

        let target_point = positions[target];

        if node.is_leaf() {
            if node.point == target as i32 {
                return [0.0, 0.0];
            }
            if node.point >= 0 {
                return repulsive_force(target_point, node.center_of_charge, 1.0);
            }

            let mut charge = node.charge;
            let mut center = node.center_of_charge;
            if node.contains(target_point) {
                let remaining = charge - 1.0;
                if remaining <= EPSILON {
                    return [0.0, 0.0];
                }
                center = [
                    (center[0] * charge - target_point[0]) / remaining,
                    (center[1] * charge - target_point[1]) / remaining,
                ];
                charge = remaining;
            }
            return repulsive_force(target_point, center, charge);
        }

        let dx = target_point[0] - node.center_of_charge[0];
        let dy = target_point[1] - node.center_of_charge[1];
        let dist2 = (dx * dx + dy * dy).max(EPSILON);
        let width = node.half * 2.0;

        if !node.contains(target_point) && width * width < theta * theta * dist2 {
            return repulsive_force(target_point, node.center_of_charge, node.charge);
        }

        let mut force = [0.0, 0.0];
        for child in node.children {
            if child >= 0 {
                let child_force =
                    self.repulsion_from(child as usize, target, positions, theta);
                force[0] += child_force[0];
                force[1] += child_force[1];
            }
        }
        force
    }
}

fn repulsive_force(target: Pos2, source: Pos2, source_charge: f32) -> Pos2 {
    let dx = target[0] - source[0];
    let dy = target[1] - source[1];
    let dist2 = (dx * dx + dy * dy).max(1.0);
    // Magnitude is 1/d, matching OGDF's f_rep_scalar(d)=1/d.
    [source_charge * dx / dist2, source_charge * dy / dist2]
}

// ── Disjoint set ──────────────────────────────────────────────────────────────

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
    fn parallel_edges_are_averaged_and_loops_removed() {
        let edges = simplify_edges(
            3,
            &[0, 1, 0, 2],
            &[1, 0, 1, 2],
            &[10.0, 30.0, 20.0, 50.0],
        )
        .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!((edges[0].a, edges[0].b), (0, 1));
        assert!((edges[0].desired - 20.0).abs() < 0.001);
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
    fn branched_graph_spreads_in_two_dimensions() {
        let mut edges: Vec<(u32, u32)> =
            (1..1000).map(|node| (node - 1, node)).collect();
        edges.extend((0..997).step_by(17).map(|node| (node, node + 3)));
        let output = run(1000, &edges);

        let min_x = output.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min);
        let max_x = output
            .iter()
            .map(|p| p[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = output.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
        let max_y = output
            .iter()
            .map(|p| p[1])
            .fold(f32::NEG_INFINITY, f32::max);

        assert!(max_x - min_x > 10.0);
        assert!(max_y - min_y > 10.0);
    }

    #[test]
    fn solar_coarse_edges_include_distance_to_suns() {
        let level = Level {
            node_count: 8,
            edges: (1..8)
                .map(|node| Edge {
                    a: node - 1,
                    b: node,
                    desired: 100.0,
                })
                .collect(),
            hierarchy_mass: vec![1; 8],
            prolongation: None,
        };
        let (coarse, _) = solar_coarsen(&level, 0);
        assert!(coarse.node_count < level.node_count);
        assert!(coarse.edges.iter().all(|edge| edge.desired >= 100.0));
    }

    #[test]
    fn disconnected_components_are_solved_independently() {
        let output = run(6, &[(0, 1), (1, 2), (3, 4), (4, 5)]);
        assert!(output.iter().flatten().all(|value| value.is_finite()));
    }

    #[test]
    fn exact_repulsion_pushes_apart() {
        let force = repulsive_force([10.0, 0.0], [0.0, 0.0], 1.0);
        assert!(force[0] > 0.0);
        assert!(force[1].abs() < EPSILON);
    }
}
