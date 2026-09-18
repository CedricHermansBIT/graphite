//! Selection: tracks which nodes and edges are selected.

use std::collections::HashSet;
use egui::{Pos2, Rect, Vec2};

use crate::graph::ViewGraph;
use crate::layout::Layout;

#[derive(Default, Clone)]
pub struct Selection {
    pub nodes: HashSet<usize>,
    pub edges: HashSet<(usize, usize)>,
    /// In-progress rubber-band selection.
    #[allow(dead_code)]
    pub drag_start: Option<Pos2>,
}

impl Selection {
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
    }

    #[allow(dead_code)]
    pub fn toggle_node(&mut self, idx: usize) {
        if self.nodes.contains(&idx) {
            self.nodes.remove(&idx);
        } else {
            self.nodes.insert(idx);
        }
    }

    pub fn select_node(&mut self, idx: usize, add: bool) {
        if !add { self.clear(); }
        self.nodes.insert(idx);
    }

    /// Select all nodes within a screen rectangle.
    /// Select all nodes within a screen rectangle.
    pub fn rubber_band_select(
        &mut self,
        rect: Rect,
        graph: &ViewGraph,
        layout: &Layout,
        zoom: f32,
        pan: Vec2,
        viewport_center: Pos2,
        add: bool,
    ) {
        if !add { self.clear(); }
        let num_nodes = layout.num_nodes();

        for (ni, _node) in graph.nodes.iter().enumerate() {
            if ni >= num_nodes { continue; }

            // Calculate the screen position using the node's segment center
            let c = layout.center(ni);
            let sp = Pos2::new(
                c[0] * zoom + pan.x + viewport_center.x,
                c[1] * zoom + pan.y + viewport_center.y,
            );
            if rect.contains(sp) {
                self.nodes.insert(ni);
            }
        }
    }
    /// Select all nodes in the same connected component as `start`.
    pub fn select_component(&mut self, start: usize, graph: &ViewGraph, add: bool) {
        if !add { self.clear(); }
        let adj = graph.build_adjacency();
        let mut stack = vec![start];
        let mut visited = HashSet::new();
        while let Some(v) = stack.pop() {
            if visited.contains(&v) { continue; }
            visited.insert(v);
            self.nodes.insert(v);
            for &(nb, _) in &adj[v] {
                if !visited.contains(&nb) { stack.push(nb); }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn total_length(&self, graph: &ViewGraph) -> usize {
        self.nodes.iter().map(|&ni| graph.nodes[ni].length).sum()
    }
}