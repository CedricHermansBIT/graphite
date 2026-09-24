//! Versioned, explicit sessions. Preferences contain no sequence data.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use crate::filter::FilterParams;
use crate::gfa::GfaGraph;
use crate::graph::ViewGraph;
use crate::layout::Layout;
use crate::ui::{DisplayOptions, OverlayOptions};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub display: DisplayOptions,
    pub recent_files: Vec<PathBuf>,
}

impl Preferences {
    pub fn remember(&mut self, path: PathBuf) {
        self.recent_files.retain(|old| old != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(10);
    }
}

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub format_version: u32,
    pub source: PathBuf,
    pub source_sha256: String,
    pub backend: String,
    pub filter: FilterParams,
    pub display: DisplayOptions,
    pub overlays: OverlayOptions,
    pub node_names: Vec<String>,
    pub point_counts: Vec<usize>,
    pub positions: Vec<[f32; 2]>,
    pub selection: Vec<usize>,
    pub zoom: f32,
    pub pan: [f32; 2],
}

pub fn fingerprint(gfa: &GfaGraph) -> String {
    format!("{:x}", Sha256::digest(&gfa.mmap[..]))
}

impl Session {
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).context("Cannot open session")?;
        ensure!(
            file.metadata()?.len() <= 512 * 1024 * 1024,
            "Session exceeds the 512 MiB limit"
        );
        let mut session: Self =
            serde_json::from_reader(BufReader::new(file)).context("Invalid Graphite session")?;
        ensure!(
            session.format_version == 1,
            "Unsupported session version {}",
            session.format_version
        );
        ensure!(
            session.zoom.is_finite() && (0.00001..=1000.0).contains(&session.zoom),
            "Invalid session zoom"
        );
        ensure!(
            session
                .pan
                .iter()
                .chain(session.positions.iter().flatten())
                .all(|v| v.is_finite() && v.abs() < 1.0e12),
            "Invalid session coordinates"
        );
        ensure!(
            session.point_counts.len() == session.node_names.len(),
            "Invalid session segment table"
        );
        ensure!(
            session.point_counts.iter().all(|&n| (1..=64).contains(&n)),
            "Invalid session point counts"
        );
        ensure!(
            session
                .point_counts
                .iter()
                .try_fold(0usize, |a, &b| a.checked_add(b))
                == Some(session.positions.len()),
            "Invalid session position count"
        );
        ensure!(
            session
                .selection
                .iter()
                .all(|&n| n < session.node_names.len()),
            "Invalid session selection"
        );
        ensure!(
            valid_display(&session.display),
            "Invalid session display settings"
        );
        ensure!(
            session
                .filter
                .min_depth
                .into_iter()
                .chain(session.filter.max_depth)
                .all(|v| v.is_finite() && v >= 0.0),
            "Invalid session depth filter"
        );
        if session.source.is_relative() {
            session.source = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&session.source);
        }
        Ok(session)
    }

    pub fn validate_graph(&self, gfa: &GfaGraph, view: &ViewGraph, layout: &Layout) -> Result<()> {
        ensure!(
            self.source_sha256 == fingerprint(gfa),
            "The source GFA has changed since this session was saved"
        );
        ensure!(
            self.node_names.len() == view.nodes.len()
                && self
                    .node_names
                    .iter()
                    .zip(&view.nodes)
                    .all(|(name, node)| name == node.name.as_ref()),
            "Session segments do not match the filtered graph"
        );
        ensure!(
            self.point_counts == layout.node_pts_count,
            "Session geometry is incompatible with this layout version"
        );
        ensure!(
            self.overlays
                .selected_path
                .is_none_or(|n| n < gfa.paths.len())
                && self
                    .overlays
                    .selected_walk
                    .is_none_or(|n| n < gfa.walks.len()),
            "Invalid session overlay selection"
        );
        Ok(())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        ensure_distinct_output(path, &self.source)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut temporary, self)?;
        temporary.flush()?;
        ensure!(
            temporary.as_file().metadata()?.len() <= 512 * 1024 * 1024,
            "Session exceeds the 512 MiB limit"
        );
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

pub fn valid_display(display: &DisplayOptions) -> bool {
    display.node_scale.is_finite()
        && (0.2..=5.0).contains(&display.node_scale)
        && display.edge_opacity.is_finite()
        && (0.0..=1.0).contains(&display.edge_opacity)
        && display.min_depth_color.is_finite()
        && display.max_depth_color.is_finite()
}

/// Never truncate the memory-mapped input, even through a symlink or hard link.
pub fn ensure_distinct_output(output: &Path, input: &Path) -> Result<()> {
    if output.exists() {
        ensure!(
            !same_file::is_same_file(output, input)?,
            "Choose an output file different from the source GFA"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_files_are_unique_and_bounded() {
        let mut preferences = Preferences::default();
        for n in 0..15 {
            preferences.remember(PathBuf::from(format!("{n}.gfa")));
        }
        preferences.remember(PathBuf::from("12.gfa"));
        assert_eq!(preferences.recent_files.len(), 10);
        assert_eq!(preferences.recent_files[0], Path::new("12.gfa"));
        assert_eq!(
            preferences
                .recent_files
                .iter()
                .filter(|p| **p == Path::new("12.gfa"))
                .count(),
            1
        );
    }

    #[test]
    fn rejects_overwriting_source_and_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.gfa");
        std::fs::write(&source, "S\ta\tACGT\n").unwrap();
        let alias = directory.path().join("alias.fa");
        std::fs::hard_link(&source, &alias).unwrap();
        assert!(ensure_distinct_output(&source, &source).is_err());
        assert!(ensure_distinct_output(&alias, &source).is_err());
        assert!(ensure_distinct_output(&directory.path().join("new.fa"), &source).is_ok());
    }
}
