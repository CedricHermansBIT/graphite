#include "ogdf/energybased/FMMMLayout.h"
#include <cstddef>
#include <cstdint>
#include <cmath>
#include <mutex>
#include <vector>

// OGDF's random seed and legacy internals are process-global. Serialize calls;
// interactive Rust layout and rendering remain independent of this lock.
extern "C" int bandage_initial_layout(std::size_t count, std::size_t edge_count,
    const uint32_t *from, const uint32_t *to, const float *length, float *xy) noexcept {
    static std::mutex mutex;
    std::lock_guard<std::mutex> guard(mutex);
    try {
        ogdf::Graph graph;
        std::vector<ogdf::node> nodes;
        nodes.reserve(count);
        for (std::size_t i=0; i<count; ++i) nodes.push_back(graph.newNode());
        ogdf::GraphAttributes attributes(graph, ogdf::GraphAttributes::nodeGraphics | ogdf::GraphAttributes::edgeGraphics);
        ogdf::EdgeArray<double> lengths(graph);
        for (std::size_t i=0; i<edge_count; ++i) {
            if (from[i] >= count || to[i] >= count) return 2;
            if (from[i] == to[i]) continue;
            auto edge = graph.newEdge(nodes[from[i]], nodes[to[i]]);
            lengths[edge] = std::max(1.0, static_cast<double>(length[i]));
        }
        ogdf::FMMMLayout layout;
        // Bandage/program/graphlayoutworker.cpp, quality 1 (fast).
        layout.randSeed(100);
        layout.useHighLevelOptions(false);
        layout.initialPlacementForces(ogdf::FMMMLayout::ipfRandomRandIterNr);
        layout.unitEdgeLength(1.0);
        layout.allowedPositions(ogdf::FMMMLayout::apAll);
        layout.pageRatio(1.0);
        layout.minDistCC(520.0);
        layout.stepsForRotatingComponents(50);
        layout.fixedIterations(count > 50000 ? 3 : 12);
        layout.fineTuningIterations(count > 50000 ? 1 : 8);
        layout.nmPrecision(2);
        layout.call(attributes, lengths);
        for (std::size_t i=0; i<count; ++i) {
            const double x = attributes.x(nodes[i]), y = attributes.y(nodes[i]);
            if (!std::isfinite(x) || !std::isfinite(y)) return 3;
            xy[2*i] = static_cast<float>(x);
            xy[2*i+1] = static_cast<float>(y);
        }
        return 0;
    } catch (...) { return 1; }
}
