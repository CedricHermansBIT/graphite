//! GFA1 parser optimised for large files (100 MB+).
//!
//! Strategy:
//!  - Memory-map the file so the OS handles paging.
//!  - Byte-level scans with ranges into the mmap for large textual fields.
//!  - Keep optional tags in one flat zero-copy table.
//!  - Resolve segment references only after all S records have been indexed.
//!  - Store path/walk steps in flat arrays rather than one allocation per record.

use std::{
    collections::HashMap,
    fs::File,
    ops::Range,
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result};
use memmap2::Mmap;

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

impl Tag {
    pub fn name_eq(&self, name: &[u8; 2]) -> bool {
        self.name[0].eq_ignore_ascii_case(&name[0])
            && self.name[1].eq_ignore_ascii_case(&name[1])
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    /// Range into GfaGraph::tags.
    pub tag_range: Range<usize>,
}

/// A segment (S line). The sequence is stored as a byte-range into the mmap.
#[derive(Debug, Clone)]
pub struct Segment {
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
    pub overlap_range: Range<usize>,
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
    pub tag_range: Range<usize>,
}

/// A GFA1 containment (C line).
///
/// Containments are preserved in the data model but are not converted into
/// ordinary endpoint-to-endpoint display links: position can attach the
/// contained segment inside the container.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStep {
    pub segment: usize,
    pub strand: Strand,
    /// How this step connects to the next step. None on the final step.
    pub connection_to_next: Option<PathConnection>,
}

/// A GFA1 path (P line).
#[derive(Debug, Clone)]
pub struct GfaPath {
    pub name: Arc<str>,
    /// Range into GfaGraph::path_steps.
    pub steps: Range<usize>,
    /// Raw overlap/distance list in the mmap; empty for *.
    pub overlaps_range: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrientedSegment {
    pub segment: usize,
    pub strand: Strand,
}

/// A GFA1.1 walk (W line).
#[derive(Debug, Clone)]
pub struct Walk {
    pub sample_id: Arc<str>,
    pub haplotype_index: usize,
    pub sequence_id: Arc<str>,
    pub sequence_start: Option<u64>,
    pub sequence_end: Option<u64>,
    /// Range into GfaGraph::walk_steps.
    pub steps: Range<usize>,
}

/// The parsed GFA graph.
pub struct GfaGraph {
    /// Memory-mapped file kept alive as long as the graph lives.
    pub mmap: Mmap,
    pub version: GfaVersion,
    pub headers: Vec<Header>,
    pub segments: Vec<Segment>,
    /// Number of segments with an embedded nucleotide sequence.
    pub sequence_segment_count: usize,
    pub links: Vec<Link>,
    pub jumps: Vec<Jump>,
    pub containments: Vec<Containment>,
    pub paths: Vec<GfaPath>,
    pub path_steps: Vec<PathStep>,
    pub walks: Vec<Walk>,
    pub walk_steps: Vec<OrientedSegment>,
    /// Flat optional-tag storage. Record structs index this with tag_range.
    pub tags: Vec<Tag>,
    /// Name -> segment index lookup.
    pub name_index: HashMap<Arc<str>, usize>,
}

impl GfaGraph {
    pub fn segment_sequence(&self, seg: &Segment) -> &[u8] {
        seg.sequence(&self.mmap)
    }

    pub fn segment_sequence_str(&self, seg: &Segment) -> String {
        String::from_utf8_lossy(seg.sequence(&self.mmap)).into_owned()
    }

    pub fn total_sequence_length(&self) -> usize {
        self.segments.iter().map(|s| s.length).sum()
    }

    pub fn n50(&self) -> usize {
        let total = self.total_sequence_length();
        let mut lengths: Vec<usize> = self.segments.iter().map(|s| s.length).collect();
        lengths.sort_unstable_by(|a, b| b.cmp(a));
        let mut cumsum = 0usize;
        for l in &lengths {
            cumsum += l;
            if cumsum * 2 >= total {
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
}

// ── Parser ───────────────────────────────────────────────────────────────────

pub fn parse_gfa<P: AsRef<Path>>(path: P) -> Result<GfaGraph> {
    let file = File::open(&path)
        .with_context(|| format!("Cannot open {:?}", path.as_ref()))?;
    let mmap = unsafe { Mmap::map(&file) }.context("mmap failed")?;

    parse_gfa_bytes(mmap)
}

fn parse_gfa_bytes(mmap: Mmap) -> Result<GfaGraph> {
    let bytes = &mmap[..];
    let version = detect_gfa_version(bytes);

    if version == GfaVersion::Gfa2_0
        || (version == GfaVersion::Unspecified && looks_like_unheaded_gfa2(bytes))
    {
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
    let mut name_index: HashMap<Arc<str>, usize> = HashMap::new();

    let mut pos = 0usize;
    while pos < bytes.len() {
        let line_start = pos;
        while pos < bytes.len() && bytes[pos] != b'\n' {
            pos += 1;
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

        match line[0] {
            b'H' => headers.push(parse_h_line(line, line_start, &mut tags)),
            b'S' => {
                let id = segments.len();
                if let Some(seg) = parse_s_line(line, line_start, &mmap, id, &mut tags) {
                    if name_index.insert(seg.name.clone(), id).is_some() {
                        log::warn!("duplicate GFA segment name '{}'; later segment wins", seg.name);
                    }
                    segments.push(seg);
                }
            }
            b'L' => {
                if let Some(link) = parse_l_line(line, line_start, &mut tags) {
                    links_raw.push(link);
                }
            }
            b'J' => {
                if let Some(jump) = parse_j_line(line, line_start, &mut tags) {
                    jumps_raw.push(jump);
                }
            }
            b'C' => {
                if let Some(containment) = parse_c_line(line, line_start, &mut tags) {
                    containments_raw.push(containment);
                }
            }
            b'P' => {
                if let Some(path) = parse_p_line(line, line_start, &mut raw_path_steps) {
                    paths_raw.push(path);
                }
            }
            b'W' => {
                if let Some(walk) = parse_w_line(line, line_start, &mut raw_walk_steps) {
                    walks_raw.push(walk);
                }
            }
            _ => {}
        }
    }

    let mut skipped_links = 0usize;
    let mut links = Vec::with_capacity(links_raw.len());
    for raw in links_raw {
        let (Some(from), Some(to)) = (
            resolve_name(&mmap, &raw.from_name, &name_index),
            resolve_name(&mmap, &raw.to_name, &name_index),
        ) else {
            skipped_links += 1;
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
    for raw in jumps_raw {
        let (Some(from), Some(to)) = (
            resolve_name(&mmap, &raw.from_name, &name_index),
            resolve_name(&mmap, &raw.to_name, &name_index),
        ) else {
            skipped_jumps += 1;
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
    for raw in containments_raw {
        let (Some(container), Some(contained)) = (
            resolve_name(&mmap, &raw.container_name, &name_index),
            resolve_name(&mmap, &raw.contained_name, &name_index),
        ) else {
            skipped_containments += 1;
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
    for raw in paths_raw {
        let start = path_steps.len();
        let mut valid = true;
        for step in &raw_path_steps[raw.steps.clone()] {
            let Some(segment) = resolve_name(&mmap, &step.name_range, &name_index) else {
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
            });
        } else {
            path_steps.truncate(start);
            skipped_paths += 1;
        }
    }

    let mut skipped_walks = 0usize;
    let mut walks = Vec::with_capacity(walks_raw.len());
    let mut walk_steps = Vec::with_capacity(raw_walk_steps.len());
    for raw in walks_raw {
        let start = walk_steps.len();
        let mut valid = true;
        for step in &raw_walk_steps[raw.steps.clone()] {
            let Some(segment) = resolve_name(&mmap, &step.name_range, &name_index) else {
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
            });
        } else {
            walk_steps.truncate(start);
            skipped_walks += 1;
        }
    }

    if skipped_links + skipped_jumps + skipped_containments + skipped_paths + skipped_walks > 0 {
        log::warn!(
            "GFA references to unknown segments skipped: {skipped_links} links, {skipped_jumps} jumps, {skipped_containments} containments, {skipped_paths} paths, {skipped_walks} walks"
        );
    }

    let sequence_segment_count = segments
        .iter()
        .filter(|segment| !segment.seq_range.is_empty())
        .count();

    Ok(GfaGraph {
        mmap,
        version,
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

fn detect_gfa_version(bytes: &[u8]) -> GfaVersion {
    for mut line in bytes.split(|&b| b == b'\n') {
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.first() != Some(&b'H') {
            continue;
        }
        for field in tab_fields(line).skip(1) {
            if field.len() >= 5
                && field[0].eq_ignore_ascii_case(&b'V')
                && field[1].eq_ignore_ascii_case(&b'N')
                && field[2] == b':'
                && field[3].eq_ignore_ascii_case(&b'Z')
                && field[4] == b':'
            {
                return parse_version_value(&field[5..]);
            }
        }
    }
    GfaVersion::Unspecified
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
    if field.len() < 5 || field[2] != b':' || field[4] != b':' {
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
    let sequence_length = if seq_bytes == b"*" { 0 } else { seq_bytes.len() };

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
                seg_length = value.parse().unwrap_or(sequence_length);
            }
            (b"DP", b'f' | b'i', Some(value)) => {
                dp_depth = value.parse().ok();
            }
            (b"RD", b'f' | b'i', Some(value)) => {
                read_depth = value.parse().ok();
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

fn parse_c_line(
    line: &[u8],
    line_start: usize,
    tags: &mut Vec<Tag>,
) -> Option<RawContainment> {
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

    Some(RawPath {
        name,
        steps: start..raw_steps.len(),
        overlaps_range: non_star_range(line, line_start, overlaps_field),
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
                b',' => Some(PathConnection::Link),
                b';' => Some(PathConnection::Jump),
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

    Some(RawWalk {
        sample_id,
        haplotype_index,
        sequence_id,
        sequence_start,
        sequence_end,
        steps: start..raw_steps.len(),
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

fn resolve_name(
    mmap: &[u8],
    name_range: &Range<usize>,
    name_index: &HashMap<Arc<str>, usize>,
) -> Option<usize> {
    let name = std::str::from_utf8(&mmap[name_range.clone()]).ok()?;
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
        let graph = parse(
            b"H\tVN:Z:1.2\tTS:i:10\nS\ts1\tACGT\tFC:i:7\tKC:i:12\n",
        );
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
        let error = parse_gfa_bytes(mmap_from(
            b"H\tVN:Z:2.0\nS\ts1\t4\tACGT\n",
        ))
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
        let graph = parse(
            b"H\tVN:Z:1.0\nS\ta\tAAAA\nS\tb\tAAAT\nL\ta\t+\tb\t-\t3M\tMQ:i:60\n",
        );
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
        let graph = parse(
            b"H\tVN:Z:1.2\nS\ta\tA\nS\tb\tT\nJ\ta\t+\tb\t-\t1000\tSC:i:1\n",
        );
        assert_eq!(graph.jumps.len(), 1);
        let jump = &graph.jumps[0];
        assert_eq!(jump.distance, Some(1000));
        assert!(jump.shortcut);
        assert_eq!(jump.from, 0);
        assert_eq!(jump.to, 1);
    }

    #[test]
    fn parses_paths_with_link_and_jump_separators() {
        let graph = parse(
            b"H\tVN:Z:1.2\nS\ta\tA\nS\tb\tT\nS\tc\tG\nP\tp1\ta+,b-;c+\t1M,10J\n",
        );
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
        assert_eq!(steps[0], OrientedSegment { segment: 0, strand: Strand::Forward });
        assert_eq!(steps[1], OrientedSegment { segment: 1, strand: Strand::Reverse });
        assert_eq!(steps[2], OrientedSegment { segment: 2, strand: Strand::Forward });
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
}
