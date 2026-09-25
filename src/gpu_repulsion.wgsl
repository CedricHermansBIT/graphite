struct QuadNode {
    bounds: vec4<f32>,
    mass: vec4<f32>,
    range: vec4<u32>,
    children: vec4<i32>,
};
struct Params {
    count: u32,
    theta: f32,
    padding: vec2<u32>,
};
@group(0) @binding(0) var<storage, read> nodes: array<QuadNode>;
@group(0) @binding(1) var<storage, read> ordered: array<u32>;
@group(0) @binding(2) var<storage, read> positions: array<vec2<f32>>;
@group(0) @binding(3) var<uniform> params: Params;
@group(0) @binding(4) var<storage, read_write> forces: array<vec2<f32>>;

fn pair_force(target_pos: vec2<f32>, source: vec2<f32>, charge: f32) -> vec2<f32> {
    let delta = target_pos - source;
    let dist2 = max(dot(delta, delta), 1.0);
    return charge * delta / dist2;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let target_index = gid.x;
    if target_index >= params.count { return; }
    let target_pos = positions[target_index];
    var result = vec2<f32>(0.0);
    // A depth-28 quadtree has at most 1 + 3*28 pending siblings in DFS.
    var stack: array<i32, 128>;
    var top = 1u;
    stack[0] = 0;
    loop {
        if top == 0u { break; }
        top -= 1u;
        let node = nodes[u32(stack[top])];
        let charge = node.bounds.w;
        if charge <= 0.00001 { continue; }
        let contained = all(abs(target_pos - node.bounds.xy) <= vec2<f32>(node.bounds.z));
        if all(node.children == vec4<i32>(-1)) {
            let count = node.range.y - node.range.x;
            if count <= 32u {
                for (var slot = node.range.x; slot < node.range.y; slot += 1u) {
                    let source_index = ordered[slot];
                    if source_index != target_index {
                        result += pair_force(target_pos, positions[source_index], 1.0);
                    }
                }
            } else {
                var leaf_charge = charge;
                var center = node.mass.xy;
                if contained {
                    leaf_charge -= 1.0;
                    if leaf_charge <= 0.00001 { continue; }
                    center = (center * charge - target_pos) / leaf_charge;
                }
                result += pair_force(target_pos, center, leaf_charge);
            }
            continue;
        }
        let delta = target_pos - node.mass.xy;
        let dist2 = max(dot(delta, delta), 0.00001);
        let width = node.bounds.z * 2.0;
        if !contained && width * width < params.theta * params.theta * dist2 {
            result += pair_force(target_pos, node.mass.xy, charge);
            continue;
        }
        for (var quadrant = 3i; quadrant >= 0i; quadrant -= 1i) {
            let child = node.children[u32(quadrant)];
            if child >= 0 {
                stack[top] = child;
                top += 1u;
            }
        }
    }
    forces[target_index] = result;
}
