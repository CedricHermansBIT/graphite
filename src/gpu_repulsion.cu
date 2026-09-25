struct Node {
    float bounds[4];
    float mass[4];
    unsigned range[4];
    int children[4];
};
struct Pos { float x, y; };

__device__ Pos pair_force(Pos target, Pos source, float charge) {
    float dx = target.x - source.x;
    float dy = target.y - source.y;
    float dist2 = fmaxf(dx * dx + dy * dy, 1.0f);
    return {charge * dx / dist2, charge * dy / dist2};
}

extern "C" __global__ void repulsion(
    const Node *nodes, const unsigned *ordered, const Pos *positions,
    Pos *forces, unsigned count, float theta
) {
    unsigned target_index = blockIdx.x * blockDim.x + threadIdx.x;
    if (target_index >= count) return;
    Pos target = positions[target_index];
    Pos result = {0.0f, 0.0f};
    // A depth-28 quadtree has at most 1 + 3*28 pending siblings in DFS.
    int stack[128];
    int top = 1;
    stack[0] = 0;
    while (top > 0) {
        const Node &node = nodes[stack[--top]];
        float charge = node.bounds[3];
        if (charge <= 0.00001f) continue;
        bool contained = fabsf(target.x - node.bounds[0]) <= node.bounds[2]
                      && fabsf(target.y - node.bounds[1]) <= node.bounds[2];
        bool leaf = node.children[0] < 0 && node.children[1] < 0
                 && node.children[2] < 0 && node.children[3] < 0;
        if (leaf) {
            unsigned leaf_count = node.range[1] - node.range[0];
            if (leaf_count <= 32) {
                for (unsigned slot = node.range[0]; slot < node.range[1]; ++slot) {
                    unsigned source_index = ordered[slot];
                    if (source_index == target_index) continue;
                    Pos force = pair_force(target, positions[source_index], 1.0f);
                    result.x += force.x;
                    result.y += force.y;
                }
            } else {
                float leaf_charge = charge;
                Pos center = {node.mass[0], node.mass[1]};
                if (contained) {
                    leaf_charge -= 1.0f;
                    if (leaf_charge <= 0.00001f) continue;
                    center.x = (center.x * charge - target.x) / leaf_charge;
                    center.y = (center.y * charge - target.y) / leaf_charge;
                }
                Pos force = pair_force(target, center, leaf_charge);
                result.x += force.x;
                result.y += force.y;
            }
            continue;
        }
        float dx = target.x - node.mass[0];
        float dy = target.y - node.mass[1];
        float dist2 = fmaxf(dx * dx + dy * dy, 0.00001f);
        float width = node.bounds[2] * 2.0f;
        if (!contained && width * width < theta * theta * dist2) {
            Pos center = {node.mass[0], node.mass[1]};
            Pos force = pair_force(target, center, charge);
            result.x += force.x;
            result.y += force.y;
            continue;
        }
        for (int quadrant = 3; quadrant >= 0; --quadrant) {
            int child = node.children[quadrant];
            if (child >= 0) stack[top++] = child;
        }
    }
    forces[target_index] = result;
}
