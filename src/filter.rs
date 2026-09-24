//! Filtering: which segments appear in the view graph.

use crate::gfa::Segment;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ColorMode {
    Depth,
    Length,
    Uniform,
    ReadCount,
}

impl ColorMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Depth => "Depth / coverage",
            Self::Length => "Length",
            Self::Uniform => "Uniform",
            Self::ReadCount => "Read count",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentTopology {
    All,
    Circular,
    Linear,
    Branched,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentSort {
    SegmentCount,
    TotalLength,
    MeanDepth,
    TotalReadCount,
}

impl ComponentSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::SegmentCount => "Number of segments",
            Self::TotalLength => "Total length",
            Self::MeanDepth => "Mean depth / coverage",
            Self::TotalReadCount => "Total read count",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentSortOrder {
    Descending,
    Ascending,
}

impl ComponentSortOrder {
    pub fn label(self) -> &'static str {
        match self {
            Self::Descending => "Highest first",
            Self::Ascending => "Lowest first",
        }
    }
}

impl ComponentTopology {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All topologies",
            Self::Circular => "Circular only",
            Self::Linear => "Linear only",
            Self::Branched => "Branched only",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct FilterParams {
    /// Minimum sequence length to show (bp).
    pub min_length: usize,
    /// Maximum sequence length to show (bp); None = no limit.
    pub max_length: Option<usize>,
    /// Minimum depth coverage to show; None = no limit.
    pub min_depth: Option<f64>,
    /// Maximum depth coverage to show; None = no limit.
    pub max_depth: Option<f64>,
    /// Name substring filter (case-insensitive).
    pub name_contains: String,
    /// Show only the first N components in the selected order; 0 = all.
    pub top_components: usize,
    /// Metric used to order components and select the top N.
    pub component_sort: ComponentSort,
    /// Direction used to order components.
    pub component_sort_order: ComponentSortOrder,
    /// Component topology to retain.
    pub component_topology: ComponentTopology,
    /// Minimum number of segments in a connected component.
    pub min_component_segments: usize,
    /// Maximum number of segments in a connected component; None = no limit.
    pub max_component_segments: Option<usize>,
}

impl Default for FilterParams {
    fn default() -> Self {
        Self {
            min_length: 0,
            max_length: None,
            min_depth: None,
            max_depth: None,
            name_contains: String::new(),
            top_components: 0,
            component_sort: ComponentSort::SegmentCount,
            component_sort_order: ComponentSortOrder::Descending,
            component_topology: ComponentTopology::All,
            min_component_segments: 1,
            max_component_segments: None,
        }
    }
}

impl FilterParams {
    /// Returns true if the segment should be included.
    pub fn accepts(&self, seg: &Segment) -> bool {
        if seg.length < self.min_length {
            return false;
        }
        if let Some(max_l) = self.max_length
            && seg.length > max_l
        {
            return false;
        }
        if let Some(d) = seg.depth {
            if let Some(min_d) = self.min_depth
                && d < min_d
            {
                return false;
            }
            if let Some(max_d) = self.max_depth
                && d > max_d
            {
                return false;
            }
        }
        if !self.name_contains.is_empty() {
            let matches = if seg.name.is_ascii() && self.name_contains.is_ascii() {
                seg.name
                    .as_bytes()
                    .windows(self.name_contains.len())
                    .any(|window| window.eq_ignore_ascii_case(self.name_contains.as_bytes()))
            } else {
                seg.name
                    .to_lowercase()
                    .contains(&self.name_contains.to_lowercase())
            };
            if !matches {
                return false;
            }
        }
        true
    }

    /// Describes active filters as a short string.
    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.min_length > 0 {
            parts.push(format!("len≥{}", self.min_length));
        }
        if let Some(max_l) = self.max_length {
            parts.push(format!("len≤{}", max_l));
        }
        if let Some(d) = self.min_depth {
            parts.push(format!("depth≥{:.1}", d));
        }
        if let Some(d) = self.max_depth {
            parts.push(format!("depth≤{:.1}", d));
        }
        if !self.name_contains.is_empty() {
            parts.push(format!("name~\"{}\"", self.name_contains));
        }
        if self.top_components > 0 {
            parts.push(format!("top-{} components", self.top_components));
        }
        if self.component_topology != ComponentTopology::All {
            parts.push(self.component_topology.label().to_string());
        }
        if self.min_component_segments > 1 {
            parts.push(format!(
                "component segments≥{}",
                self.min_component_segments
            ));
        }
        if let Some(max) = self.max_component_segments {
            parts.push(format!("component segments≤{max}"));
        }
        if parts.is_empty() {
            "None".to_string()
        } else {
            parts.join(", ")
        }
    }
}
