//! GFA parser optimised for large files (100 MB+).
//!
//! Strategy:
//!  - Memory-map the file so the OS handles paging.
//!  - Single-pass byte-level scan; no heap allocations per field.
//!  - Segments (S lines) are stored with their sequence as a byte-range
//!    into the mmap so we never copy the sequence bytes unless the user
//!    requests them explicitly.
//!  - Links (L lines) reference segment indices, not names, after an
//!    O(n) name→index resolution step.

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
    #[allow(dead_code)]
    pub fn as_char(self) -> char {
        match self {
            Self::Forward => '+',
            Self::Reverse => '-',
        }
    }
}

/// A segment (S line).  The sequence is stored as a byte-range into the mmap.
#[derive(Debug, Clone)]
pub struct Segment {
    #[allow(dead_code)]
    pub id: usize,
    pub name: Arc<str>,
    /// Range into `GfaGraph::mmap` bytes; empty if sequence is `*`.
    pub seq_range: Range<usize>,
    pub length: usize,
    /// Optional depth / coverage from DP (Flye) or rd (hifiasm).
    pub depth: Option<f64>,
    /// Optional RC tag (read count).
    pub read_count: Option<u64>,
}

impl Segment {
    pub fn sequence<'a>(&self, mmap: &'a [u8]) -> &'a [u8] {
        &mmap[self.seq_range.clone()]
    }
}

/// A link (L line).
#[derive(Debug, Clone)]
pub struct Link {
    pub from: usize,
    pub from_strand: Strand,
    pub to: usize,
    pub to_strand: Strand,
    /// Overlap CIGAR (byte-range into mmap, or empty for `*`).
    #[allow(dead_code)]
    pub overlap_range: Range<usize>,
}

/// The parsed GFA graph.
pub struct GfaGraph {
    /// Memory-mapped file kept alive as long as the graph lives.
    pub mmap: Mmap,
    pub segments: Vec<Segment>,
    /// Number of segments with an embedded nucleotide sequence.
    pub sequence_segment_count: usize,
    pub links: Vec<Link>,
    /// Name → segment index lookup.
    #[allow(dead_code)]
    pub name_index: HashMap<Arc<str>, usize>,
}

#[allow(dead_code)]
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
}

// ── Parser ───────────────────────────────────────────────────────────────────

pub fn parse_gfa<P: AsRef<Path>>(path: P) -> Result<GfaGraph> {
    let file = File::open(&path)
        .with_context(|| format!("Cannot open {:?}", path.as_ref()))?;
    let mmap = unsafe { Mmap::map(&file) }
        .context("mmap failed")?;

    parse_gfa_bytes(mmap)
}

fn parse_gfa_bytes(mmap: Mmap) -> Result<GfaGraph> {
    let bytes = &mmap[..];

    let mut segments: Vec<Segment> = Vec::new();
    let mut links_raw: Vec<(Arc<str>, Strand, Arc<str>, Strand, Range<usize>)> = Vec::new();
    let mut name_index: HashMap<Arc<str>, usize> = HashMap::new();

    let mut pos = 0usize;
    let len = bytes.len();

    while pos < len {
        // Find end of line
        let line_start = pos;
        while pos < len && bytes[pos] != b'\n' {
            pos += 1;
        }
        let line_end = pos;
        if pos < len {
            pos += 1; // consume '\n'
        }

        let line = &bytes[line_start..line_end];
        if line.is_empty() || line[0] == b'#' {
            continue;
        }

        match line[0] {
            b'S' => {
                if let Some(seg) = parse_s_line(line, line_start, &mmap) {
                    let idx = segments.len();
                    name_index.insert(seg.name.clone(), idx);
                    segments.push(seg);
                }
            }
            b'L' => {
                if let Some(lnk) = parse_l_line(line, line_start) {
                    links_raw.push(lnk);
                }
            }
            _ => {} // H, P, W, C lines ignored for now
        }
    }

    // Resolve link names → indices
    let mut links: Vec<Link> = Vec::with_capacity(links_raw.len());
    for (from_name, from_strand, to_name, to_strand, ovlp) in links_raw {
        let Some(&from) = name_index.get(&from_name) else { continue };
        let Some(&to) = name_index.get(&to_name) else { continue };
        links.push(Link { from, from_strand, to, to_strand, overlap_range: ovlp });
    }

    let sequence_segment_count = segments
        .iter()
        .filter(|segment| !segment.seq_range.is_empty())
        .count();
    Ok(GfaGraph {
        mmap,
        segments,
        sequence_segment_count,
        links,
        name_index,
    })
}

// ── Line parsers ─────────────────────────────────────────────────────────────

/// Returns (fields split by tab) as byte-slices into `line`.
fn tab_fields(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|&b| b == b'\t')
}

fn parse_s_line(line: &[u8], _line_start: usize, mmap: &[u8]) -> Option<Segment> {
    let mut fields = tab_fields(line);
    fields.next(); // 'S'
    let name_bytes = fields.next()?;
    let seq_bytes = fields.next()?;

    let name: Arc<str> = std::str::from_utf8(name_bytes).ok()?.into();

    // Locate sequence inside the full mmap for zero-copy storage.
    // seq_bytes is a sub-slice of the line which is a sub-slice of mmap.
    let seq_range = if seq_bytes == b"*" {
        0..0
    } else {
        let offset = unsafe {
            seq_bytes.as_ptr().offset_from(mmap.as_ptr()) as usize
        };
        offset..offset + seq_bytes.len()
    };
    let length = if seq_bytes == b"*" { 0 } else { seq_bytes.len() };

    // Parse optional tags: LN:i:, dp:f:, rd:i:, RC:i:
    let mut seg_length = length;
    let mut dp_depth: Option<f64> = None;
    let mut read_depth: Option<f64> = None;
    let mut read_count: Option<u64> = None;

    for tag in fields {
        if tag.len() < 5 || tag[2] != b':' || tag[4] != b':' { continue; }
        let tag_name = [tag[0].to_ascii_uppercase(), tag[1].to_ascii_uppercase()];
        let tag_type = tag[3].to_ascii_lowercase();
        let tag_val_bytes = &tag[5..];
        let Ok(val_str) = std::str::from_utf8(tag_val_bytes) else { continue };

        match (&tag_name, tag_type) {
            (b"LN", b'i') => {
                seg_length = val_str.parse().unwrap_or(length);
            }
            (b"DP", b'f' | b'i') => {
                dp_depth = val_str.parse().ok();
            }
            // hifiasm's read coverage tag. Keep DP as the preferred value if
            // a producer supplies both tags on the same segment.
            (b"RD", b'f' | b'i') => {
                read_depth = val_str.parse().ok();
            }
            (b"RC", b'i') => {
                read_count = val_str.parse().ok();
            }
            _ => {}
        }
    }

    let depth = dp_depth.or(read_depth);
    Some(Segment { id: 0, name, seq_range, length: seg_length, depth, read_count })
}

fn parse_l_line(
    line: &[u8],
    _line_start: usize,
) -> Option<(Arc<str>, Strand, Arc<str>, Strand, Range<usize>)> {
    let mut fields = tab_fields(line);
    fields.next(); // 'L'
    let from_name: Arc<str> = std::str::from_utf8(fields.next()?).ok()?.into();
    let from_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let to_name: Arc<str> = std::str::from_utf8(fields.next()?).ok()?.into();
    let to_strand = Strand::from_byte(*fields.next()?.first()?)?;
    let _overlap = fields.next().unwrap_or(b"*");

    Some((from_name, from_strand, to_name, to_strand, 0..0))
}

#[cfg(test)]
mod tests {
    use super::parse_s_line;

    #[test]
    fn parses_hifiasm_read_depth() {
        let line = b"S\th1tg000001l\t*\tLN:i:9865\trd:i:3";
        let segment = parse_s_line(line, 0, line).unwrap();

        assert_eq!(segment.length, 9865);
        assert_eq!(segment.depth, Some(3.0));
    }

    #[test]
    fn dp_takes_precedence_over_rd_regardless_of_tag_order() {
        for line in [
            b"S\tnode\t*\tDP:f:8.5\trd:i:3".as_slice(),
            b"S\tnode\t*\trd:i:3\tDP:f:8.5".as_slice(),
        ] {
            let segment = parse_s_line(line, 0, line).unwrap();
            assert_eq!(segment.depth, Some(8.5));
        }
    }
}
