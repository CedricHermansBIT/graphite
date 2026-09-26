//! GFA1 parser optimised for large files (100 MB+).
//!
//! Strategy:
//!  - Memory-map the file so the OS handles paging.
//!  - Byte-level scans with ranges into the mmap for large textual fields.
//!  - Keep optional tags in one flat zero-copy table.
//!  - Resolve segment references only after all S records have been indexed.
//!  - Store path/walk steps in flat arrays rather than one allocation per record.

use std::{
    io::Read,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::AHashMap;
use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;
#[cfg(not(target_arch = "wasm32"))]
use memmap2::Mmap;
use std::ops::Deref;
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::Path,
};

/// Backing bytes for range-based GFA records. Native files remain memory mapped.
pub enum InputBytes {
    #[cfg(not(target_arch = "wasm32"))]
    Mmap(Mmap),
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Owned(Vec<u8>),
}

impl Deref for InputBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mmap(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<Mmap> for InputBytes {
    fn from(value: Mmap) -> Self {
        Self::Mmap(value)
    }
}

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strand {
    Forward,
    Reverse,
}

impl Strand {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            b'+' => Some(Self::Forward),
            b'-' => Some(Self::Reverse),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn as_char(self) -> char {
        match self {
            Self::Forward => '+',
            Self::Reverse => '-',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GfaVersion {
    Unspecified,
    Gfa1_0,
    Gfa1_1,
    Gfa1_2,
    Gfa2_0,
    Other,
}

impl GfaVersion {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Gfa1_0 => "1.0",
            Self::Gfa1_1 => "1.1",
            Self::Gfa1_2 => "1.2",
            Self::Gfa2_0 => "2.0",
            Self::Other => "other",
        }
    }
}

/// Generic optional GFA tag. The value remains zero-copy in the mapped input.
#[derive(Debug, Clone)]
pub struct Tag {
    pub name: [u8; 2],
    pub value_type: u8,
    pub value_range: Range<usize>,
}

#[allow(dead_code)]
impl Tag {
    pub fn name_eq(&self, name: &[u8; 2]) -> bool {
        self.name[0].eq_ignore_ascii_case(&name[0]) && self.name[1].eq_ignore_ascii_case(&name[1])
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    /// Range into GfaGraph::tags.
    #[allow(dead_code)]
    pub tag_range: Range<usize>,
}

/// A segment (S line). The sequence is stored as a byte-range into the mmap.
#[derive(Debug, Clone)]
pub struct Segment {
    #[allow(dead_code)]
    pub id: usize,
    pub name: Arc<str>,
    /// Range into GfaGraph::mmap; empty if sequence is *.
    pub seq_range: Range<usize>,
    pub length: usize,
    /// Optional depth / coverage from DP (Flye) or rd (hifiasm).
    pub depth: Option<f64>,
    /// Optional RC tag (read count).
    pub read_count: Option<u64>,
    /// All optional tags, including tags Graphite interprets explicitly.
    #[allow(dead_code)]
    pub tag_range: Range<usize>,
}

impl Segment {
    pub fn sequence<'a>(&self, mmap: &'a [u8]) -> &'a [u8] {
        &mmap[self.seq_range.clone()]
    }
}

/// A standard GFA1 link (L line).
#[derive(Debug, Clone)]
pub struct Link {
    pub from: usize,
    pub from_strand: Strand,
    pub to: usize,
    pub to_strand: Strand,
    /// Overlap CIGAR in the mmap; empty for *.
    #[allow(dead_code)]
    pub overlap_range: Range<usize>,
    #[allow(dead_code)]
    pub tag_range: Range<usize>,
}

/// A GFA1.2 jump connection (J line).
#[derive(Debug, Clone)]
pub struct Jump {
    pub from: usize,
    pub from_strand: Strand,
    pub to: usize,
    pub to_strand: Strand,
    pub distance: Option<i64>,
    /// SC:i:1 marks a shortcut jump.
    pub shortcut: bool,
    #[allow(dead_code)]
    pub tag_range: Range<usize>,
}

/// A GFA1 containment (C line).
///
/// Containments are preserved in the data model but are not converted into
/// ordinary endpoint-to-endpoint display links: position can attach the
/// contained segment inside the container.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Containment {
    pub container: usize,
    pub container_strand: Strand,
    pub contained: usize,
    pub contained_strand: Strand,
    pub position: usize,
    /// CIGAR in the mmap; empty for *.
    pub overlap_range: Range<usize>,
    pub tag_range: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathConnection {
    Link,
    Jump,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStep {
    pub segment: usize,
    pub strand: Strand,
    /// How this step connects to the next step. None on the final step.
    pub connection_to_next: Option<PathConnection>,
}

/// A GFA1 path (P line).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct GfaPath {
    pub name: Arc<str>,
    /// Range into GfaGraph::path_steps.
    pub steps: Range<usize>,
    /// Raw overlap/distance list in the mmap; empty for *.
    pub overlaps_range: Range<usize>,
    pub tag_range: Range<usize>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrientedSegment {
    pub segment: usize,
    pub strand: Strand,
}

/// A GFA1.1 walk (W line).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Walk {
    pub sample_id: Arc<str>,
    pub haplotype_index: usize,
    pub sequence_id: Arc<str>,
    pub sequence_start: Option<u64>,
    pub sequence_end: Option<u64>,
    /// Range into GfaGraph::walk_steps.
    pub steps: Range<usize>,
    pub tag_range: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
}

/// A recoverable input problem. Line numbers are one-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub line: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

/// The parsed GFA graph.
pub struct GfaGraph {
    /// Memory-mapped file kept alive as long as the graph lives.
    pub mmap: InputBytes,
    pub version: GfaVersion,
    pub diagnostics: Vec<ParseDiagnostic>,
    #[allow(dead_code)]
    pub headers: Vec<Header>,
    pub segments: Vec<Segment>,
    /// Number of segments with an embedded nucleotide sequence.
    pub sequence_segment_count: usize,
    pub links: Vec<Link>,
    pub jumps: Vec<Jump>,
    pub containments: Vec<Containment>,
    pub paths: Vec<GfaPath>,
    #[allow(dead_code)]
    pub path_steps: Vec<PathStep>,
    pub walks: Vec<Walk>,
    #[allow(dead_code)]
    pub walk_steps: Vec<OrientedSegment>,
    /// Flat optional-tag storage. Record structs index this with tag_range.
    pub tags: Vec<Tag>,
    /// Name -> segment index lookup.
    #[allow(dead_code)]
    pub name_index: AHashMap<Arc<str>, usize>,
}

#[allow(dead_code)]
impl GfaGraph {
    pub fn diagnostic_summary(&self) -> Option<String> {
        self.diagnostics.first().map(|first| {
            format!(
                "{} input warning(s); line {}: {}",
                if self.diagnostics.len() > MAX_DIAGNOSTICS {
                    format!("more than {MAX_DIAGNOSTICS}")
                } else {
                    self.diagnostics.len().to_string()
                },
                first.line,
                first.message
            )
        })
    }

    pub fn segment_sequence(&self, seg: &Segment) -> &[u8] {
        seg.sequence(&self.mmap)
    }

    pub fn segment_sequence_str(&self, seg: &Segment) -> String {
        String::from_utf8_lossy(seg.sequence(&self.mmap)).into_owned()
    }

    pub fn total_sequence_length(&self) -> usize {
        self.segments
            .iter()
            .fold(0usize, |total, s| total.saturating_add(s.length))
    }

    pub fn n50(&self) -> usize {
        let total: u128 = self.segments.iter().map(|s| s.length as u128).sum();
        let mut lengths: Vec<usize> = self.segments.iter().map(|s| s.length).collect();
        lengths.sort_unstable_by(|a, b| b.cmp(a));
        let mut cumsum = 0u128;
        for l in &lengths {
            cumsum += *l as u128;
            if cumsum >= total.div_ceil(2) {
                return *l;
            }
        }
        0
    }

    pub fn record_tags(&self, range: &Range<usize>) -> &[Tag] {
        &self.tags[range.clone()]
    }

    pub fn tag_value(&self, tag: &Tag) -> &[u8] {
        &self.mmap[tag.value_range.clone()]
    }

    pub fn path_steps(&self, path: &GfaPath) -> &[PathStep] {
        &self.path_steps[path.steps.clone()]
    }

    pub fn walk_steps(&self, walk: &Walk) -> &[OrientedSegment] {
        &self.walk_steps[walk.steps.clone()]
    }

    pub fn link_overlap(&self, link: &Link) -> &[u8] {
        &self.mmap[link.overlap_range.clone()]
    }

    pub fn containment_overlap(&self, containment: &Containment) -> &[u8] {
        &self.mmap[containment.overlap_range.clone()]
    }
}

// ── Internal unresolved records ──────────────────────────────────────────────

#[derive(Debug)]
struct RawLink {
    from_name: Range<usize>,
    from_strand: Strand,
    to_name: Range<usize>,
    to_strand: Strand,
    overlap_range: Range<usize>,
    tag_range: Range<usize>,
}

#[derive(Debug)]
struct RawJump {
    from_name: Range<usize>,
    from_strand: Strand,
    to_name: Range<usize>,
    to_strand: Strand,
    distance: Option<i64>,
    shortcut: bool,
    tag_range: Range<usize>,
}

#[derive(Debug)]
struct RawContainment {
    container_name: Range<usize>,
    container_strand: Strand,
    contained_name: Range<usize>,
    contained_strand: Strand,
    position: usize,
    overlap_range: Range<usize>,
    tag_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct RawPathStep {
    name_range: Range<usize>,
    strand: Strand,
    connection_to_next: Option<PathConnection>,
}

#[derive(Debug)]
struct RawPath {
    name: Arc<str>,
    steps: Range<usize>,
    overlaps_range: Range<usize>,
    tag_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct RawWalkStep {
    name_range: Range<usize>,
    strand: Strand,
}

#[derive(Debug)]
struct RawWalk {
    sample_id: Arc<str>,
    haplotype_index: usize,
    sequence_id: Arc<str>,
    sequence_start: Option<u64>,
    sequence_end: Option<u64>,
    steps: Range<usize>,
    tag_range: Range<usize>,
}

// ── Parser ───────────────────────────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub fn parse_gfa<P: AsRef<Path>>(path: P) -> Result<GfaGraph> {
    parse_gfa_with_control(path, &AtomicBool::new(false))
}

/// Plain inputs stay memory-mapped. Gzip inputs are inflated into an anonymous
/// temporary file, keeping decompressed bytes out of the process heap.
#[cfg(not(target_arch = "wasm32"))]
pub fn parse_gfa_with_control<P: AsRef<Path>>(path: P, cancel: &AtomicBool) -> Result<GfaGraph> {
    check_cancelled(cancel)?;
    let mut file = File::open(&path).with_context(|| format!("Cannot open {:?}", path.as_ref()))?;
    let mut magic = [0u8; 2];
    let count = file.read(&mut magic).context("Cannot read GFA input")?;
    file.seek(SeekFrom::Start(0))?;
    if count == 2 && magic == [0x1f, 0x8b] {
        file = decompress_gzip(file, cancel, MAX_DECOMPRESSED_BYTES)?;
    }
    anyhow::ensure!(file.metadata()?.len() > 0, "GFA input is empty");
    let mmap = unsafe { Mmap::map(&file) }.context("Cannot memory-map GFA input")?;
    parse_gfa_bytes_with_control(mmap.into(), cancel)
}

/// Browser input budget leaves room for graph structures and layout in shared Wasm memory.
#[cfg(target_arch = "wasm32")]
pub const MAX_WEB_GFA_BYTES: usize = 256 * 1024 * 1024;
#[cfg(target_arch = "wasm32")]
const MAX_WEB_SEGMENTS: usize = 500_000;
#[cfg(target_arch = "wasm32")]
const MAX_WEB_CONNECTIONS: usize = 1_000_000;
#[cfg(target_arch = "wasm32")]
const MAX_WEB_PATH_WALK_STEPS: usize = 2_000_000;

/// Parse browser-owned bytes; gzip is decoded in bounded chunks before parsing.
#[cfg(target_arch = "wasm32")]
pub fn parse_gfa_owned(mut bytes: Vec<u8>) -> Result<GfaGraph> {
    anyhow::ensure!(
        bytes.len() <= MAX_WEB_GFA_BYTES,
        "GFA exceeds the browser 256 MiB input limit; open it in desktop Graphite"
    );
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = MultiGzDecoder::new(bytes.as_slice());
        let mut decoded = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = decoder
                .read(&mut buffer)
                .context("Cannot decompress gzip GFA input")?;
            if count == 0 {
                break;
            }
            anyhow::ensure!(
                decoded.len() <= MAX_WEB_GFA_BYTES - count,
                "Decompressed GFA exceeds the browser 256 MiB limit; open it in desktop Graphite"
            );
            decoded
                .try_reserve(count)
                .context("Browser memory is full while decompressing GFA")?;
            decoded.extend_from_slice(&buffer[..count]);
        }
        bytes = decoded;
    }
    anyhow::ensure!(!bytes.is_empty(), "GFA input is empty");
    let mut segments = 0usize;
    let mut connections = 0usize;
    let mut path_walk_steps = 0usize;
    for line in bytes.split(|&byte| byte == b'\n') {
        if line.get(1) != Some(&b'\t') {
            continue;
        }
        match line[0] {
            b'S' => segments += 1,
            b'L' | b'J' | b'C' => connections += 1,
            b'P' => {
                if let Some(field) = tab_fields(line).nth(2)
                    && !field.is_empty()
                    && field != b"*"
                {
                    path_walk_steps = path_walk_steps
                        .saturating_add(1 + memchr::memchr_iter(b',', field).count());
                }
            }
            b'W' => {
                if let Some(field) = tab_fields(line).nth(6) {
                    path_walk_steps = path_walk_steps.saturating_add(
                        field.iter().filter(|&&byte| byte == b'>' || byte == b'<').count(),
                    );
                }
            }
            _ => {}
        }
        anyhow::ensure!(
            segments <= MAX_WEB_SEGMENTS && connections <= MAX_WEB_CONNECTIONS,
            "GFA has too many records for the browser (limit: 500,000 segments and 1,000,000 connections); open it in desktop Graphite"
        );
        anyhow::ensure!(
            path_walk_steps <= MAX_WEB_PATH_WALK_STEPS,
            "GFA paths and walks contain too many steps for the browser (limit: 2,000,000); open it in desktop Graphite"
        );
    }
    parse_gfa_bytes_with_control(InputBytes::Owned(bytes), &AtomicBool::new(false))
}

#[cfg(not(target_arch = "wasm32"))]
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 1000;

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "GFA loading cancelled");
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn decompress_gzip<R: Read>(file: R, cancel: &AtomicBool, limit: u64) -> Result<File> {
    check_cancelled(cancel)?;
    let mut decoder = MultiGzDecoder::new(file);
    let mut output = tempfile::tempfile().context("Cannot create temporary file for gzip input")?;
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        check_cancelled(cancel)?;
        let count = decoder
            .read(&mut buffer)
            .context("Cannot decompress gzip GFA input")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        anyhow::ensure!(
            total <= limit,
            "Decompressed GFA exceeds the {} GiB safety limit",
            limit / (1024 * 1024 * 1024)
        );
        output
            .write_all(&buffer[..count])
            .context("Cannot write decompressed GFA temporary file")?;
    }
    check_cancelled(cancel)?;
    output.flush()?;
    output.seek(SeekFrom::Start(0))?;
    Ok(output)
}

fn warn(diagnostics: &mut Vec<ParseDiagnostic>, line: usize, message: impl Into<String>) {
    if diagnostics.len() < MAX_DIAGNOSTICS {
        diagnostics.push(ParseDiagnostic {
            line,
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
        });
    } else if diagnostics.len() == MAX_DIAGNOSTICS {
        diagnostics.push(ParseDiagnostic {
            line,
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "Further warnings omitted after the first {MAX_DIAGNOSTICS} input problems"
            ),
        });
    }
}

#[cfg(test)]
fn parse_gfa_bytes(mmap: Mmap) -> Result<GfaGraph> {
    parse_gfa_bytes_with_control(mmap.into(), &AtomicBool::new(false))
}

fn parse_gfa_bytes_with_control(mmap: InputBytes, cancel: &AtomicBool) -> Result<GfaGraph> {
    check_cancelled(cancel)?;
    let bytes = &mmap[..];
    let version = detect_gfa_version(bytes, cancel)?;

    if version == GfaVersion::Gfa2_0 {
        anyhow::bail!(
            "GFA2 input detected. Graphite currently supports GFA1 records; GFA2 S/E/F/G/O/U records require a separate parser."
        );
    }

    let mut headers = Vec::new();
    let mut segments = Vec::new();
    let mut links_raw = Vec::new();
    let mut jumps_raw = Vec::new();
    let mut containments_raw = Vec::new();
    let mut paths_raw = Vec::new();
    let mut raw_path_steps = Vec::new();
    let mut walks_raw = Vec::new();
    let mut raw_walk_steps = Vec::new();
    let mut tags = Vec::new();
    let mut name_index: AHashMap<Arc<str>, usize> = AHashMap::new();
    let mut diagnostics = Vec::new();

    let mut pos = 0usize;
    let mut line_number = 0usize;
    while pos < bytes.len() {
        check_cancelled(cancel)?;
        line_number += 1;
        let line_start = pos;
        // Bound each SIMD search so cancellation still responds on very long
        // path or sequence records.
        while pos < bytes.len() {
            let end = (pos + 64 * 1024).min(bytes.len());
            if let Some(offset) = memchr::memchr(b'\n', &bytes[pos..end]) {
                pos += offset;
                break;
            }
            pos = end;
            check_cancelled(cancel)?;
        }
        let mut line_end = pos;
        if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        if pos < bytes.len() {
            pos += 1;
        }

        let line = &bytes[line_start..line_end];
        if line.is_empty() || line[0] == b'#' {
            continue;
        }

        if version == GfaVersion::Unspecified && line[0] == b'S' && looks_like_unheaded_gfa2(line) {
            anyhow::bail!(
                "GFA2 input detected on line {line_number}. Graphite currently supports GFA1 records."
            );
        }
        if let Some(problem) = validate_record(line) {
            warn(&mut diagnostics, line_number, problem);
            continue;
        }
        validate_optional_fields(line, line_number, &mut diagnostics);

        match line[0] {
            b'H' => headers.push(parse_h_line(line, line_start, &mut tags)),
            b'S' => {
                let id = segments.len();
                if let Some(seg) = parse_s_line(line, line_start, &mmap, id, &mut tags) {
                    anyhow::ensure!(
                        name_index.insert(seg.name.clone(), id).is_none(),
                        "Duplicate GFA segment name '{}' on line {line_number}",
                        seg.name
                    );
                    segments.push(seg);
                } else {
                    warn(&mut diagnostics, line_number, "Malformed S record skipped");
                }
            }
            b'L' => {
                if let Some(link) = parse_l_line(line, line_start, &mut tags) {
                    links_raw.push((line_number, link));
                } else {
                    warn(&mut diagnostics, line_number, "Malformed L record skipped");
                }
            }
            b'J' => {
                if let Some(jump) = parse_j_line(line, line_start, &mut tags) {
                    jumps_raw.push((line_number, jump));
                } else {
                    warn(&mut diagnostics, line_number, "Malformed J record skipped");
                }
            }
            b'C' => {
                if let Some(containment) = parse_c_line(line, line_start, &mut tags) {
                    containments_raw.push((line_number, containment));
                } else {
                    warn(&mut diagnostics, line_number, "Malformed C record skipped");
                }
            }
            b'P' => {
                if let Some(path) = parse_p_line(line, line_start, &mut raw_path_steps, &mut tags) {
                    paths_raw.push((line_number, path));
                } else {
                    warn(&mut diagnostics, line_number, "Malformed P record skipped");
                }
            }
            b'W' => {
                if let Some(walk) = parse_w_line(line, line_start, &mut raw_walk_steps, &mut tags) {
                    walks_raw.push((line_number, walk));
                } else {
                    warn(&mut diagnostics, line_number, "Malformed W record skipped");
                }
            }
            _ => {}
        }
    }

    let numeric_name_index = build_numeric_name_index(&segments);
    let numeric_names = numeric_name_index.as_deref();
    let mut skipped_links = 0usize;
    let mut links = Vec::with_capacity(links_raw.len());
    for (line_number, raw) in links_raw {
        check_cancelled(cancel)?;
        let (Some(from), Some(to)) = (
            resolve_name(&mmap, &raw.from_name, &name_index, numeric_names),
            resolve_name(&mmap, &raw.to_name, &name_index, numeric_names),
        ) else {
            skipped_links += 1;
            warn(
                &mut diagnostics,
                line_number,
                "L record skipped: reference to an unknown segment",
            );
            continue;
        };
        links.push(Link {
            from,
            from_strand: raw.from_strand,
            to,
            to_strand: raw.to_strand,
            overlap_range: raw.overlap_range,
            tag_range: raw.tag_range,
        });
    }

    let mut skipped_jumps = 0usize;
    let mut jumps = Vec::with_capacity(jumps_raw.len());
    for (line_number, raw) in jumps_raw {
        check_cancelled(cancel)?;
        let (Some(from), Some(to)) = (
            resolve_name(&mmap, &raw.from_name, &name_index, numeric_names),
            resolve_name(&mmap, &raw.to_name, &name_index, numeric_names),
        ) else {
            skipped_jumps += 1;
            warn(
                &mut diagnostics,
                line_number,
                "J record skipped: reference to an unknown segment",
            );
            continue;
        };
        jumps.push(Jump {
            from,
            from_strand: raw.from_strand,
            to,
            to_strand: raw.to_strand,
            distance: raw.distance,
            shortcut: raw.shortcut,
            tag_range: raw.tag_range,
        });
    }

    let mut skipped_containments = 0usize;
    let mut containments = Vec::with_capacity(containments_raw.len());
    for (line_number, raw) in containments_raw {
        check_cancelled(cancel)?;
        let (Some(container), Some(contained)) = (
            resolve_name(&mmap, &raw.container_name, &name_index, numeric_names),
            resolve_name(&mmap, &raw.contained_name, &name_index, numeric_names),
        ) else {
            skipped_containments += 1;
            warn(
                &mut diagnostics,
                line_number,
                "C record skipped: reference to an unknown segment",
            );
            continue;
        };
        containments.push(Containment {
            container,
            container_strand: raw.container_strand,
            contained,
            contained_strand: raw.contained_strand,
            position: raw.position,
            overlap_range: raw.overlap_range,
            tag_range: raw.tag_range,
        });
    }

    let mut skipped_paths = 0usize;
    let mut paths = Vec::with_capacity(paths_raw.len());
    let mut path_steps = Vec::with_capacity(raw_path_steps.len());
    for (line_number, raw) in paths_raw {
        check_cancelled(cancel)?;
        let start = path_steps.len();
        let mut valid = true;
        for (step_index, step) in raw_path_steps[raw.steps.clone()].iter().enumerate() {
            if step_index & 0xffff == 0 {
                check_cancelled(cancel)?;
            }
            let Some(segment) = resolve_name(&mmap, &step.name_range, &name_index, numeric_names)
            else {
                valid = false;
                break;
            };
            path_steps.push(PathStep {
                segment,
                strand: step.strand,
                connection_to_next: step.connection_to_next,
            });
        }
        if valid {
            paths.push(GfaPath {
                name: raw.name,
                steps: start..path_steps.len(),
                overlaps_range: raw.overlaps_range,
                tag_range: raw.tag_range,
            });
        } else {
            path_steps.truncate(start);
            skipped_paths += 1;
            warn(
                &mut diagnostics,
                line_number,
                "P record skipped: reference to an unknown segment",
            );
        }
    }

    let mut skipped_walks = 0usize;
    let mut walks = Vec::with_capacity(walks_raw.len());
    let mut walk_steps = Vec::with_capacity(raw_walk_steps.len());
    for (line_number, raw) in walks_raw {
        check_cancelled(cancel)?;
        let start = walk_steps.len();
        let mut valid = true;
        for (step_index, step) in raw_walk_steps[raw.steps.clone()].iter().enumerate() {
            if step_index & 0xffff == 0 {
                check_cancelled(cancel)?;
            }
            let Some(segment) = resolve_name(&mmap, &step.name_range, &name_index, numeric_names)
            else {
                valid = false;
                break;
            };
            walk_steps.push(OrientedSegment {
                segment,
                strand: step.strand,
            });
        }
        if valid {
            walks.push(Walk {
                sample_id: raw.sample_id,
                haplotype_index: raw.haplotype_index,
                sequence_id: raw.sequence_id,
                sequence_start: raw.sequence_start,
                sequence_end: raw.sequence_end,
                steps: start..walk_steps.len(),
                tag_range: raw.tag_range,
            });
        } else {
            walk_steps.truncate(start);
            skipped_walks += 1;
            warn(
                &mut diagnostics,
                line_number,
                "W record skipped: reference to an unknown segment",
            );
        }
    }

    if skipped_links + skipped_jumps + skipped_containments + skipped_paths + skipped_walks > 0 {
        log::warn!(
            "GFA references to unknown segments skipped: {skipped_links} links, {skipped_jumps} jumps, {skipped_containments} containments, {skipped_paths} paths, {skipped_walks} walks"
        );
    }

    check_cancelled(cancel)?;
    anyhow::ensure!(!segments.is_empty(), "No valid GFA segments found in input");
    diagnostics.sort_by_key(|diagnostic| diagnostic.line);
    let sequence_segment_count = segments
        .iter()
        .filter(|segment| !segment.seq_range.is_empty())
        .count();

    Ok(GfaGraph {
        mmap,
        version,
        diagnostics,
        headers,
        segments,
        sequence_segment_count,
        links,
        jumps,
        containments,
        paths,
        path_steps,
        walks,
        walk_steps,
        tags,
        name_index,
    })
}

// ── Version detection ────────────────────────────────────────────────────────

fn parse_version_value(value: &[u8]) -> GfaVersion {
    match value {
        b"1.0" => GfaVersion::Gfa1_0,
        b"1.1" => GfaVersion::Gfa1_1,
        b"1.2" => GfaVersion::Gfa1_2,
        b"2.0" => GfaVersion::Gfa2_0,
        _ => GfaVersion::Other,
    }
}

fn detect_gfa_version(bytes: &[u8], cancel: &AtomicBool) -> Result<GfaVersion> {
    for mut line in bytes.split(|&b| b == b'\n') {
        check_cancelled(cancel)?;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        if line.first() != Some(&b'H') {
            // Header records conventionally precede graph records. Stopping
            // here avoids a full extra pass over large headerless GFA1 files.
            break;
        }
        for field in tab_fields(line).skip(1) {
            if field.len() >= 5
                && field[0].eq_ignore_ascii_case(&b'V')
                && field[1].eq_ignore_ascii_case(&b'N')
                && field[2] == b':'
                && field[3].eq_ignore_ascii_case(&b'Z')
                && field[4] == b':'
            {
                return Ok(parse_version_value(&field[5..]));
            }
        }
    }
    Ok(GfaVersion::Unspecified)
}

fn looks_like_unheaded_gfa2(bytes: &[u8]) -> bool {
    for mut line in bytes.split(|&b| b == b'\n') {
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.first() != Some(&b'S') {
            continue;
        }
        let mut fields = tab_fields(line);
        let _ = fields.next();
        let _name = fields.next();
        let Some(third) = fields.next() else {
            continue;
        };
        let Some(fourth) = fields.next() else {
            continue;
        };
        // GFA1's third field is a sequence or *, never an integer length.
        // GFA2's S record is S <id> <length> <sequence>.
        if !third.is_empty()
            && third.iter().all(u8::is_ascii_digit)
            && (fourth == b"*" || !fourth.starts_with(b"LN:"))
        {
            return true;
        }
    }
    false
}

// ── Line parsers ─────────────────────────────────────────────────────────────

/// Validate required fields before the permissive record parsers run, so a
/// truncated record cannot quietly acquire invented defaults.
fn validate_record(line: &[u8]) -> Option<String> {
    let record = *line.first()?;
    if !matches!(record, b'H' | b'S' | b'L' | b'J' | b'C' | b'P' | b'W') {
        return None;
    }
    let result = (|| -> std::result::Result<(), &'static str> {
        let mut fields = tab_fields(line);
        if fields.next() != Some(&line[..1]) {
            return Err("record type must be one character followed by a tab");
        }
        let mut next = || fields.next().ok_or("missing required field");
        match record {
            b'H' => {}
            b'S' => {
                if !valid_name(next()?) {
                    return Err("invalid segment name");
                }
                let sequence = next()?;
                if sequence != b"*"
                    && !sequence
                        .iter()
                        .all(|b| b.is_ascii_alphabetic() || matches!(b, b'=' | b'.'))
                {
                    return Err("invalid segment sequence");
                }
            }
            b'L' | b'J' | b'C' => {
                if !valid_name(next()?) {
                    return Err("invalid source segment name");
                }
                if !matches!(next()?, b"+" | b"-") {
                    return Err("invalid source orientation");
                }
                if !valid_name(next()?) {
                    return Err("invalid target segment name");
                }
                if !matches!(next()?, b"+" | b"-") {
                    return Err("invalid target orientation");
                }
                if record == b'C'
                    && std::str::from_utf8(next()?)
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .is_none()
                {
                    return Err("invalid containment position");
                }
                let value = next()?;
                if record == b'J' {
                    if value != b"*"
                        && std::str::from_utf8(value)
                            .ok()
                            .and_then(|v| v.parse::<i64>().ok())
                            .is_none()
                    {
                        return Err("invalid jump distance");
                    }
                } else if !valid_cigar(value) {
                    return Err("invalid overlap CIGAR");
                }
            }
            b'P' => {
                if !valid_name(next()?) {
                    return Err("invalid path name");
                }
                next()?;
                if next()?.is_empty() {
                    return Err("empty path overlap list");
                }
            }
            b'W' => {
                if next()?.is_empty() {
                    return Err("empty walk sample ID");
                }
                if std::str::from_utf8(next()?)
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .is_none()
                {
                    return Err("invalid haplotype index");
                }
                if next()?.is_empty() {
                    return Err("empty walk sequence ID");
                }
                let start = parse_optional_u64(next()?).ok_or("invalid walk start coordinate")?;
                let end = parse_optional_u64(next()?).ok_or("invalid walk end coordinate")?;
                if matches!((start, end), (Some(a), Some(b)) if a > b) {
                    return Err("walk end precedes start");
                }
                next()?;
            }
            _ => unreachable!(),
        }
        Ok(())
    })();
    result
        .err()
        .map(|reason| format!("Malformed {} record skipped: {reason}", record as char))
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !matches!(name[0], b'*' | b'=')
        && name.iter().all(|&b| (33..=126).contains(&b))
        && !name.windows(2).any(|pair| pair == b"+," || pair == b"-,")
}

fn valid_cigar(value: &[u8]) -> bool {
    if value == b"*" {
        return true;
    }
    let mut digits = false;
    for &byte in value {
        if byte.is_ascii_digit() {
            digits = true;
        } else if digits
            && matches!(
                byte,
                b'M' | b'I' | b'D' | b'N' | b'S' | b'H' | b'P' | b'=' | b'X'
            )
        {
            digits = false;
        } else {
            return false;
        }
    }
    !value.is_empty() && !digits
}

fn validate_optional_fields(
    line: &[u8],
    line_number: usize,
    diagnostics: &mut Vec<ParseDiagnostic>,
) {
    let required = match line[0] {
        b'H' => 1,
        b'S' => 3,
        b'L' | b'J' => 6,
        b'C' | b'W' => 7,
        b'P' => 4,
        _ => return,
    };
    let sequence = (line[0] == b'S').then(|| tab_fields(line).nth(2).unwrap());
    if sequence.is_some_and(|sequence| sequence.is_empty()) {
        warn(
            diagnostics,
            line_number,
            "Empty segment sequence treated as missing; use '*' for a sequence-less segment",
        );
    }
    for field in tab_fields(line).skip(required) {
        if field.len() < 5
            || field[2] != b':'
            || field[4] != b':'
            || !field[0].is_ascii_alphabetic()
            || !field[1].is_ascii_alphanumeric()
            || !matches!(field[3], b'A' | b'i' | b'f' | b'Z' | b'J' | b'H' | b'B')
        {
            warn(diagnostics, line_number, "Malformed optional tag ignored");
            continue;
        }
        let value = std::str::from_utf8(&field[5..]).ok();
        if field[..2].eq_ignore_ascii_case(b"LN") && field[3] == b'i' && sequence.is_some() {
            match value.and_then(|v| v.parse::<usize>().ok()) {
                Some(length)
                    if sequence.is_some_and(|seq| {
                        !seq.is_empty() && seq != b"*" && seq.len() != length
                    }) =>
                {
                    warn(
                        diagnostics,
                        line_number,
                        "LN tag disagrees with embedded sequence length; using the sequence length",
                    );
                }
                None => warn(diagnostics, line_number, "Invalid LN length ignored"),
                _ => {}
            }
        }
        if (field[..2].eq_ignore_ascii_case(b"DP") || field[..2].eq_ignore_ascii_case(b"RD"))
            && matches!(field[3], b'f' | b'i')
            && value
                .and_then(|v| v.parse::<f64>().ok())
                .is_none_or(|v| !v.is_finite() || v < 0.0)
        {
            warn(
                diagnostics,
                line_number,
                "Invalid depth value ignored (expected a finite nonnegative number)",
            );
        }
    }
}

fn tab_fields(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|&b| b == b'\t')
}

fn range_in_line(line: &[u8], line_start: usize, field: &[u8]) -> Range<usize> {
    let relative = unsafe { field.as_ptr().offset_from(line.as_ptr()) as usize };
    let start = line_start + relative;
    start..start + field.len()
}

fn non_star_range(line: &[u8], line_start: usize, field: &[u8]) -> Range<usize> {
    if field == b"*" {
        0..0
    } else {
        range_in_line(line, line_start, field)
    }
}

fn parse_tag_field(line: &[u8], line_start: usize, field: &[u8]) -> Option<Tag> {
    if field.len() < 5
        || field[2] != b':'
        || field[4] != b':'
        || !field[0].is_ascii_alphabetic()
        || !field[1].is_ascii_alphanumeric()
        || !matches!(field[3], b'A' | b'i' | b'f' | b'Z' | b'J' | b'H' | b'B')
    {
        return None;
    }
    Some(Tag {
        name: [field[0], field[1]],
        value_type: field[3],
        value_range: range_in_line(line, line_start, &field[5..]),
    })
}

fn parse_h_line(line: &[u8], line_start: usize, tags: &mut Vec<Tag>) -> Header {
    let start = tags.len();
    for field in tab_fields(line).skip(1) {
        if let Some(tag) = parse_tag_field(line, line_start, field) {
            tags.push(tag);
        }
    }
    Header {
        tag_range: start..tags.len(),
    }
}

fn parse_s_line(
    line: &[u8],
    line_start: usize,
    mmap: &[u8],
    id: usize,
    tags: &mut Vec<Tag>,
) -> Option<Segment> {
    let mut fields = tab_fields(line);
    fields.next();
    let name_bytes = fields.next()?;
    let seq_bytes = fields.next()?;

    let name: Arc<str> = std::str::from_utf8(name_bytes).ok()?.into();
    let seq_range = non_star_range(line, line_start, seq_bytes);
    let sequence_length = if seq_bytes == b"*" {
        0
    } else {
        seq_bytes.len()
    };

    let mut seg_length = sequence_length;
    let mut dp_depth = None;
    let mut read_depth = None;
    let mut read_count = None;
    let tag_start = tags.len();

    for field in fields {
        let Some(tag) = parse_tag_field(line, line_start, field) else {
            continue;
        };
        let value = &mmap[tag.value_range.clone()];
        let value_str = std::str::from_utf8(value).ok();
        let upper_name = [
            tag.name[0].to_ascii_uppercase(),
            tag.name[1].to_ascii_uppercase(),
        ];
        let value_type = tag.value_type.to_ascii_lowercase();

        match (&upper_name, value_type, value_str) {
            (b"LN", b'i', Some(value)) => {
                if seq_bytes == b"*" || seq_bytes.is_empty() {
                    seg_length = value.parse().unwrap_or(sequence_length);
                }
            }
            (b"DP", b'f' | b'i', Some(value)) => {
                dp_depth = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0);
            }
            (b"RD", b'f' | b'i', Some(value)) => {
                read_depth = value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0);
            }
            (b"RC", b'i', Some(value)) => {
                read_count = value.parse().ok();
            }
            _ => {}
        }
        tags.push(tag);
    }

    Some(Segment {
        id,
        name,
        seq_range,
        length: seg_length,
        depth: dp_depth.or(read_depth),
        read_count,
        tag_range: tag_start..tags.len(),
    })
}

fn parse_l_line(line: &[u8], line_start: usize, tags: &mut Vec<Tag>) -> Option<RawLink> {
    let mut fields = tab_fields(line);
    fields.next();
    let from_name = range_in_line(line, line_start, fields.next()?);
    let from_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let to_name = range_in_line(line, line_start, fields.next()?);
    let to_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let overlap = fields.next().unwrap_or(b"*");
    let overlap_range = non_star_range(line, line_start, overlap);

    let tag_start = tags.len();
    for field in fields {
        if let Some(tag) = parse_tag_field(line, line_start, field) {
            tags.push(tag);
        }
    }

    Some(RawLink {
        from_name,
        from_strand,
        to_name,
        to_strand,
        overlap_range,
        tag_range: tag_start..tags.len(),
    })
}

fn parse_j_line(line: &[u8], line_start: usize, tags: &mut Vec<Tag>) -> Option<RawJump> {
    let mut fields = tab_fields(line);
    fields.next();
    let from_name = range_in_line(line, line_start, fields.next()?);
    let from_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let to_name = range_in_line(line, line_start, fields.next()?);
    let to_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let distance_field = fields.next().unwrap_or(b"*");
    let distance = if distance_field == b"*" {
        None
    } else {
        std::str::from_utf8(distance_field).ok()?.parse().ok()
    };

    let tag_start = tags.len();
    let mut shortcut = false;
    for field in fields {
        let Some(tag) = parse_tag_field(line, line_start, field) else {
            continue;
        };
        let upper_name = [
            tag.name[0].to_ascii_uppercase(),
            tag.name[1].to_ascii_uppercase(),
        ];
        let relative_start = tag.value_range.start.checked_sub(line_start)?;
        let relative_end = tag.value_range.end.checked_sub(line_start)?;
        if &upper_name == b"SC"
            && tag.value_type.eq_ignore_ascii_case(&b'i')
            && line.get(relative_start..relative_end) == Some(b"1")
        {
            shortcut = true;
        }
        tags.push(tag);
    }

    Some(RawJump {
        from_name,
        from_strand,
        to_name,
        to_strand,
        distance,
        shortcut,
        tag_range: tag_start..tags.len(),
    })
}

fn parse_c_line(line: &[u8], line_start: usize, tags: &mut Vec<Tag>) -> Option<RawContainment> {
    let mut fields = tab_fields(line);
    fields.next();
    let container_name = range_in_line(line, line_start, fields.next()?);
    let container_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let contained_name = range_in_line(line, line_start, fields.next()?);
    let contained_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let position = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let overlap = fields.next().unwrap_or(b"*");
    let overlap_range = non_star_range(line, line_start, overlap);

    let tag_start = tags.len();
    for field in fields {
        if let Some(tag) = parse_tag_field(line, line_start, field) {
            tags.push(tag);
        }
    }

    Some(RawContainment {
        container_name,
        container_strand,
        contained_name,
        contained_strand,
        position,
        overlap_range,
        tag_range: tag_start..tags.len(),
    })
}

fn parse_p_line(
    line: &[u8],
    line_start: usize,
    raw_steps: &mut Vec<RawPathStep>,
    tags: &mut Vec<Tag>,
) -> Option<RawPath> {
    let mut fields = tab_fields(line);
    fields.next();
    let name: Arc<str> = std::str::from_utf8(fields.next()?).ok()?.into();
    let segment_field = fields.next()?;
    let overlaps_field = fields.next().unwrap_or(b"*");

    let start = raw_steps.len();
    if !parse_path_step_field(line, line_start, segment_field, raw_steps) {
        raw_steps.truncate(start);
        return None;
    }
    let tag_start = tags.len();
    tags.extend(fields.filter_map(|field| parse_tag_field(line, line_start, field)));

    Some(RawPath {
        name,
        steps: start..raw_steps.len(),
        overlaps_range: non_star_range(line, line_start, overlaps_field),
        tag_range: tag_start..tags.len(),
    })
}

fn parse_path_step_field(
    line: &[u8],
    line_start: usize,
    field: &[u8],
    output: &mut Vec<RawPathStep>,
) -> bool {
    if field.is_empty() || field == b"*" {
        return false;
    }

    let mut token_start = 0usize;
    let mut i = 0usize;
    while i <= field.len() {
        let at_end = i == field.len();
        let separator = if at_end {
            None
        } else {
            match field[i] {
                b',' if i > token_start && matches!(field[i - 1], b'+' | b'-') => {
                    Some(PathConnection::Link)
                }
                b';' if i > token_start && matches!(field[i - 1], b'+' | b'-') => {
                    Some(PathConnection::Jump)
                }
                _ => {
                    i += 1;
                    continue;
                }
            }
        };

        let token = &field[token_start..i];
        if token.len() < 2 {
            return false;
        }
        let Some(strand) = Strand::from_byte(*token.last().unwrap()) else {
            return false;
        };
        let name = &token[..token.len() - 1];
        if name.is_empty() {
            return false;
        }

        output.push(RawPathStep {
            name_range: range_in_line(line, line_start, name),
            strand,
            connection_to_next: separator,
        });

        if at_end {
            break;
        }
        token_start = i + 1;
        i += 1;
    }
    true
}

fn parse_w_line(
    line: &[u8],
    line_start: usize,
    raw_steps: &mut Vec<RawWalkStep>,
    tags: &mut Vec<Tag>,
) -> Option<RawWalk> {
    let mut fields = tab_fields(line);
    fields.next();
    let sample_id: Arc<str> = std::str::from_utf8(fields.next()?).ok()?.into();
    let haplotype_index = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let sequence_id: Arc<str> = std::str::from_utf8(fields.next()?).ok()?.into();
    let sequence_start = parse_optional_u64(fields.next()?)?;
    let sequence_end = parse_optional_u64(fields.next()?)?;
    let walk_field = fields.next()?;

    let start = raw_steps.len();
    if !parse_walk_step_field(line, line_start, walk_field, raw_steps) {
        raw_steps.truncate(start);
        return None;
    }
    let tag_start = tags.len();
    tags.extend(fields.filter_map(|field| parse_tag_field(line, line_start, field)));

    Some(RawWalk {
        sample_id,
        haplotype_index,
        sequence_id,
        sequence_start,
        sequence_end,
        steps: start..raw_steps.len(),
        tag_range: tag_start..tags.len(),
    })
}

fn parse_optional_u64(field: &[u8]) -> Option<Option<u64>> {
    if field == b"*" {
        Some(None)
    } else {
        Some(Some(std::str::from_utf8(field).ok()?.parse().ok()?))
    }
}

fn parse_walk_step_field(
    line: &[u8],
    line_start: usize,
    field: &[u8],
    output: &mut Vec<RawWalkStep>,
) -> bool {
    if field.is_empty() {
        return false;
    }

    let mut i = 0usize;
    while i < field.len() {
        let strand = match field[i] {
            b'>' => Strand::Forward,
            b'<' => Strand::Reverse,
            _ => return false,
        };
        let name_start = i + 1;
        i = name_start;
        while i < field.len() && field[i] != b'>' && field[i] != b'<' {
            i += 1;
        }
        if i == name_start {
            return false;
        }
        let name = &field[name_start..i];
        output.push(RawWalkStep {
            name_range: range_in_line(line, line_start, name),
            strand,
        });
    }
    true
}

fn canonical_decimal_id(name: &[u8]) -> Option<usize> {
    if name.is_empty() || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    name.iter().try_fold(0usize, |id, &digit| {
        digit
            .is_ascii_digit()
            .then(|| digit - b'0')
            .and_then(|digit| id.checked_mul(10)?.checked_add(digit as usize))
    })
}

fn build_numeric_name_index(segments: &[Segment]) -> Option<Vec<usize>> {
    let mut ids = Vec::with_capacity(segments.len());
    let mut max_id = 0usize;
    for segment in segments {
        let id = canonical_decimal_id(segment.name.as_bytes())?;
        max_id = max_id.max(id);
        ids.push(id);
    }
    // Sparse or very large identifiers should use the existing hash index.
    if max_id > segments.len().saturating_mul(4).saturating_add(1024) {
        return None;
    }
    let mut index = vec![usize::MAX; max_id.checked_add(1)?];
    for (segment, id) in ids.into_iter().enumerate() {
        index[id] = segment;
    }
    Some(index)
}

fn resolve_name(
    mmap: &[u8],
    name_range: &Range<usize>,
    name_index: &AHashMap<Arc<str>, usize>,
    numeric_names: Option<&[usize]>,
) -> Option<usize> {
    let bytes = &mmap[name_range.clone()];
    if let Some(index) = numeric_names {
        let id = canonical_decimal_id(bytes)?;
        return index
            .get(id)
            .copied()
            .filter(|&segment| segment != usize::MAX);
    }
    let name = std::str::from_utf8(bytes).ok()?;
    name_index.get(name).copied()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use memmap2::MmapMut;

    fn mmap_from(bytes: &[u8]) -> Mmap {
        let mut mmap = MmapMut::map_anon(bytes.len().max(1)).unwrap();
        mmap[..bytes.len()].copy_from_slice(bytes);
        mmap.make_read_only().unwrap()
    }

    fn parse(bytes: &[u8]) -> GfaGraph {
        parse_gfa_bytes(mmap_from(bytes)).unwrap()
    }

    #[test]
    fn parses_hifiasm_read_depth() {
        let line = b"S\th1tg000001l\t*\tLN:i:9865\trd:i:3";
        let mut tags = Vec::new();
        let segment = parse_s_line(line, 0, line, 0, &mut tags).unwrap();

        assert_eq!(segment.length, 9865);
        assert_eq!(segment.depth, Some(3.0));
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn dp_takes_precedence_over_rd_regardless_of_tag_order() {
        for line in [
            b"S\tnode\t*\tDP:f:8.5\trd:i:3".as_slice(),
            b"S\tnode\t*\trd:i:3\tDP:f:8.5".as_slice(),
        ] {
            let mut tags = Vec::new();
            let segment = parse_s_line(line, 0, line, 0, &mut tags).unwrap();
            assert_eq!(segment.depth, Some(8.5));
        }
    }

    #[test]
    fn detects_header_version_and_preserves_generic_tags() {
        let graph = parse(b"H\tVN:Z:1.2\tTS:i:10\nS\ts1\tACGT\tFC:i:7\tKC:i:12\n");
        assert_eq!(graph.version, GfaVersion::Gfa1_2);
        assert_eq!(graph.headers.len(), 1);
        assert_eq!(graph.segments[0].id, 0);

        let header_tags = graph.record_tags(&graph.headers[0].tag_range);
        assert_eq!(header_tags.len(), 2);
        assert_eq!(graph.tag_value(&header_tags[0]), b"1.2");

        let segment_tags = graph.record_tags(&graph.segments[0].tag_range);
        assert!(segment_tags.iter().any(|tag| tag.name_eq(b"FC")));
        assert!(segment_tags.iter().any(|tag| tag.name_eq(b"KC")));
    }

    #[test]
    fn rejects_gfa2_instead_of_misparsing_segment_length_as_sequence() {
        let error = parse_gfa_bytes(mmap_from(b"H\tVN:Z:2.0\nS\ts1\t4\tACGT\n"))
            .err()
            .expect("GFA2 should be rejected");
        assert!(error.to_string().contains("GFA2"));

        let error = parse_gfa_bytes(mmap_from(b"S\ts1\t4\tACGT\n"))
            .err()
            .expect("unheaded GFA2 should be rejected");
        assert!(error.to_string().contains("GFA2"));
    }

    #[test]
    fn preserves_link_cigar_and_tags() {
        let graph = parse(b"H\tVN:Z:1.0\nS\ta\tAAAA\nS\tb\tAAAT\nL\ta\t+\tb\t-\t3M\tMQ:i:60\n");
        assert_eq!(graph.links.len(), 1);
        let link = &graph.links[0];
        assert_eq!(graph.link_overlap(link), b"3M");
        let link_tags = graph.record_tags(&link.tag_range);
        assert_eq!(link_tags.len(), 1);
        assert!(link_tags[0].name_eq(b"MQ"));
        assert_eq!(graph.tag_value(&link_tags[0]), b"60");
    }

    #[test]
    fn parses_jump_and_shortcut_tag() {
        let graph = parse(b"H\tVN:Z:1.2\nS\ta\tA\nS\tb\tT\nJ\ta\t+\tb\t-\t1000\tSC:i:1\n");
        assert_eq!(graph.jumps.len(), 1);
        let jump = &graph.jumps[0];
        assert_eq!(jump.distance, Some(1000));
        assert!(jump.shortcut);
        assert_eq!(jump.from, 0);
        assert_eq!(jump.to, 1);
    }

    #[test]
    fn parses_paths_with_link_and_jump_separators() {
        let graph = parse(b"H\tVN:Z:1.2\nS\ta\tA\nS\tb\tT\nS\tc\tG\nP\tp1\ta+,b-;c+\t1M,10J\n");
        assert_eq!(graph.paths.len(), 1);
        let steps = graph.path_steps(&graph.paths[0]);
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].segment, 0);
        assert_eq!(steps[0].connection_to_next, Some(PathConnection::Link));
        assert_eq!(steps[1].strand, Strand::Reverse);
        assert_eq!(steps[1].connection_to_next, Some(PathConnection::Jump));
        assert_eq!(steps[2].connection_to_next, None);
        assert_eq!(
            &graph.mmap[graph.paths[0].overlaps_range.clone()],
            b"1M,10J"
        );
    }

    #[test]
    fn resolves_dense_decimal_names_without_conflating_spelling() {
        let graph =
            parse(b"S\t1\tA\nS\t2\tT\nL\t1\t+\t2\t-\t0M\nP\tgood\t1+,2-\t*\nP\tbad\t01+\t*\n");
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].from, 0);
        assert_eq!(graph.links[0].to, 1);
        assert_eq!(graph.paths.len(), 1);
        assert_eq!(graph.path_steps(&graph.paths[0])[1].segment, 1);
        assert_eq!(graph.diagnostics.len(), 1);
        assert_eq!(graph.diagnostics[0].line, 5);

        let graph = parse(b"S\t01\tA\nS\t1\tT\nP\tp\t01+,1+\t*\n");
        assert_eq!(graph.path_steps(&graph.paths[0])[0].segment, 0);
        assert_eq!(graph.path_steps(&graph.paths[0])[1].segment, 1);
    }

    #[test]
    fn parses_walk_metadata_and_orientation() {
        let graph = parse(
            b"H\tVN:Z:1.1\nS\ts11\tA\nS\ts12\tT\nS\ts13\tG\nW\tNA12878\t1\tchr1\t0\t11\t>s11<s12>s13\n",
        );
        assert_eq!(graph.walks.len(), 1);
        let walk = &graph.walks[0];
        assert_eq!(walk.sample_id.as_ref(), "NA12878");
        assert_eq!(walk.haplotype_index, 1);
        assert_eq!(walk.sequence_id.as_ref(), "chr1");
        assert_eq!(walk.sequence_start, Some(0));
        assert_eq!(walk.sequence_end, Some(11));
        let steps = graph.walk_steps(walk);
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[0],
            OrientedSegment {
                segment: 0,
                strand: Strand::Forward
            }
        );
        assert_eq!(
            steps[1],
            OrientedSegment {
                segment: 1,
                strand: Strand::Reverse
            }
        );
        assert_eq!(
            steps[2],
            OrientedSegment {
                segment: 2,
                strand: Strand::Forward
            }
        );
    }

    #[test]
    fn parses_containment_and_preserves_cigar() {
        let graph = parse(
            b"H\tVN:Z:1.0\nS\tcontainer\tAAAAAA\nS\tinside\tAAA\nC\tcontainer\t-\tinside\t+\t2\t3M\tNM:i:0\n",
        );
        assert_eq!(graph.containments.len(), 1);
        let containment = &graph.containments[0];
        assert_eq!(containment.container, 0);
        assert_eq!(containment.contained, 1);
        assert_eq!(containment.position, 2);
        assert_eq!(containment.container_strand, Strand::Reverse);
        assert_eq!(graph.containment_overlap(containment), b"3M");
        assert_eq!(graph.record_tags(&containment.tag_range).len(), 1);
    }

    #[test]
    fn crlf_input_does_not_pollute_fields() {
        let graph = parse(b"H\tVN:Z:1.0\r\nS\ta\tACGT\r\n");
        assert_eq!(graph.version, GfaVersion::Gfa1_0);
        assert_eq!(graph.segment_sequence(&graph.segments[0]), b"ACGT");
    }

    #[test]
    fn recovers_empty_sequence_producer_dialect_with_warning() {
        let graph = parse(b"H\tVN:Z:1.0\nS\tcontig\t\tLN:i:8676\tDP:f:1.0\n");
        assert_eq!(graph.segments[0].length, 8676);
        assert_eq!(graph.sequence_segment_count, 0);
        assert_eq!(graph.diagnostics.len(), 1);
        assert_eq!(graph.diagnostics[0].line, 2);
        assert!(
            graph.diagnostics[0]
                .message
                .contains("Empty segment sequence")
        );
    }

    #[test]
    fn reports_malformed_records_and_unknown_references_with_line_numbers() {
        let graph = parse(b"# comment\nS\ta\tACGT\nS\tbroken\nL\ta\t?\ta\t+\t0M\nL\ta\t+\tmissing\t+\t0M\nP\tp\ta+,missing+\t*\nW\tsample\t0\tchr\t0\t4\t>missing\n");
        assert_eq!(graph.segments.len(), 1);
        assert!(graph.links.is_empty());
        assert!(graph.paths.is_empty());
        assert!(graph.walks.is_empty());
        assert_eq!(
            graph.diagnostics.iter().map(|d| d.line).collect::<Vec<_>>(),
            vec![3, 4, 5, 6, 7]
        );
        assert!(
            graph.diagnostics[0]
                .message
                .contains("missing required field")
        );
        assert!(graph.diagnostics[2].message.contains("unknown segment"));
        assert!(
            graph
                .diagnostic_summary()
                .unwrap()
                .contains("5 input warning")
        );
    }

    #[test]
    fn rejects_duplicate_names_instead_of_retargeting_links() {
        let error = parse_gfa_bytes(mmap_from(b"S\ta\tA\nS\ta\tG\n"))
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Duplicate GFA segment name 'a' on line 2")
        );
    }

    #[test]
    fn malformed_required_fields_are_skipped_without_invented_defaults() {
        let graph = parse(b"S\ta\tA\nL\ta\t+\ta\t+\nL\ta\t++\ta\t+\t0M\nJ\ta\t+\ta\t+\tnonsense\nC\ta\t+\ta\t+\t0\tbogus\nW\ts\t0\tchr\t9\t2\t>a\n");
        assert_eq!(graph.diagnostics.len(), 5);
        assert!(
            graph.links.is_empty()
                && graph.jumps.is_empty()
                && graph.containments.is_empty()
                && graph.walks.is_empty()
        );
    }

    #[test]
    fn malformed_optional_values_do_not_corrupt_lengths_or_depth() {
        let graph = parse(b"S\ta\tACGT\tLN:i:99\tDP:f:NaN\nS\tb\t*\tLN:i:12\trd:f:-3\n");
        assert_eq!(graph.segments[0].length, 4);
        assert_eq!(graph.segments[1].length, 12);
        assert!(graph.segments.iter().all(|s| s.depth.is_none()));
        assert_eq!(graph.diagnostics.len(), 3);
    }

    #[test]
    fn retains_optional_path_and_walk_tags_and_names_with_commas() {
        let graph = parse(b"S\ta,b\tA\nS\tc\tC\nP\tp\ta,b+,c-\t*\tTP:Z:linear\nW\ts\t0\tchr\t0\t1\t>a,b\tXY:B:i,1,2\n");
        assert!(graph.diagnostics.is_empty());
        assert_eq!(graph.path_steps(&graph.paths[0]).len(), 2);
        let tag = &graph.record_tags(&graph.paths[0].tag_range)[0];
        assert_eq!(graph.tag_value(tag), b"linear");
        let tag = &graph.record_tags(&graph.walks[0].tag_range)[0];
        assert_eq!(graph.tag_value(tag), b"i,1,2");
    }

    #[test]
    fn statistics_do_not_overflow_for_large_declared_lengths() {
        let graph = parse(format!("S\ta\t*\tLN:i:{}\nS\tb\t*\tLN:i:2\n", usize::MAX).as_bytes());
        assert_eq!(graph.total_sequence_length(), usize::MAX);
        assert_eq!(graph.n50(), usize::MAX);
    }

    fn compressed(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn loads_gzip_and_concatenated_members_without_requiring_an_extension() {
        let mut input = tempfile::NamedTempFile::new().unwrap();
        input
            .write_all(&compressed(b"H\tVN:Z:1.0\nS\ta\tACGT\n"))
            .unwrap();
        input
            .write_all(&compressed(b"S\tb\tTT\nL\ta\t+\tb\t-\t0M\n"))
            .unwrap();
        input.flush().unwrap();
        let graph = parse_gfa(input.path()).unwrap();
        // The mapped decompressed data must remain alive after all source handles close.
        drop(input);
        assert_eq!(graph.segments.len(), 2);
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.segment_sequence(&graph.segments[0]), b"ACGT");
        assert!(graph.diagnostics.is_empty());
    }

    #[test]
    fn rejects_corrupt_or_oversized_gzip_data() {
        let cancel = AtomicBool::new(false);
        let bytes = compressed(b"S\ta\tACGT\n");
        let error = decompress_gzip(bytes.as_slice(), &cancel, 4).unwrap_err();
        assert!(error.to_string().contains("safety limit"));
        let error = decompress_gzip(&bytes[..bytes.len() - 3], &cancel, 1024).unwrap_err();
        assert!(error.to_string().contains("decompress"));
    }

    #[test]
    fn cancellation_stops_parsing_and_decompression() {
        let cancelled = AtomicBool::new(true);
        let error = parse_gfa_bytes_with_control(mmap_from(b"S\ta\tA\n").into(), &cancelled)
            .err()
            .unwrap();
        assert!(error.to_string().contains("cancelled"));

        struct CancelOnRead<'a> {
            bytes: &'a [u8],
            cancel: &'a AtomicBool,
        }
        impl Read for CancelOnRead<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let count = self.bytes.read(buffer)?;
                self.cancel.store(true, Ordering::Relaxed);
                Ok(count)
            }
        }
        let cancel = AtomicBool::new(false);
        let bytes = compressed(b"S\ta\tACGT\n");
        let error = decompress_gzip(
            CancelOnRead {
                bytes: &bytes,
                cancel: &cancel,
            },
            &cancel,
            1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn owned_input_preserves_the_same_ranges_as_mmap() {
        let bytes = b"S\ta\tACGT\tDP:f:4\nS\tb\tTT\nL\ta\t+\tb\t+\t0M\n";
        let mapped = parse(bytes);
        let owned = parse_gfa_bytes_with_control(
            InputBytes::Owned(bytes.to_vec()),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(owned.segments.len(), mapped.segments.len());
        assert_eq!(owned.links.len(), mapped.links.len());
        assert_eq!(
            owned.segment_sequence(&owned.segments[0]),
            mapped.segment_sequence(&mapped.segments[0])
        );
        assert_eq!(owned.segments[0].depth, mapped.segments[0].depth);
    }

    #[test]
    fn warning_storage_is_bounded_and_non_gfa_input_is_rejected() {
        let mut input = b"S\ta\tA\n".to_vec();
        for _ in 0..(MAX_DIAGNOSTICS + 50) {
            input.extend_from_slice(b"S\tbroken\n");
        }
        let graph = parse(&input);
        assert_eq!(graph.diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert!(
            graph
                .diagnostics
                .last()
                .unwrap()
                .message
                .contains("omitted")
        );
        assert!(
            parse_gfa_bytes(mmap_from(b"not a GFA file\n"))
                .err()
                .unwrap()
                .to_string()
                .contains("No valid GFA segments")
        );
    }
}
