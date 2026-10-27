// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Frontend-supplied source snapshots and exact byte ranges, separate from executable tapes.
//!
//! A map owns the full canonical encoded program identity once. Binding compares those bytes,
//! including constants/imports/contracts, before exposing source context. Span IDs are never
//! interpreted. Names are labels, not paths to open; this module performs no filesystem access.
//! The frontend owns language rules and must supply the correct document and half-open UTF-8 byte
//! range for each source instruction. Snapshots preserve their original contents; previews escape
//! terminal controls and direction-changing characters. No host/native source is inferred.
//!
//! This optional API is `no_std + alloc` and has no UI or filesystem dependency. Construct a
//! map once during compilation, bind it to an exact program, then resolve instruction locations:
//!
//! ```
//! use execution_tape::{
//!     asm::{FunctionSig, ProgramBuilder}, program::ValueType,
//!     source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping, SourcePosition},
//!     trace::TraceLocation,
//! };
//! let mut builder = ProgramBuilder::new();
//! let function = builder.declare_function(FunctionSig {
//!     arg_types: vec![], ret_types: vec![ValueType::I64],
//! })?;
//! let mut body = builder.function_asm(function)?;
//! let result = body.alloc_register(ValueType::I64);
//! let location = TraceLocation { func: function, pc: body.pc() };
//! body.const_i64(result, 42).ret(0, &[result]);
//! builder.define_function(function, body)?;
//! let program = builder.build_verified()?;
//! let sources = SourceMap::new(program.program(), vec![SourceDocument {
//!     name: "answer.expr".into(), text: "40 + 2".into(),
//! }], vec![SourceMapping { location, document: SourceId(0), range: 0..6 }],
//! SourceLimits::default())?;
//! let bound = sources.bind(program.program())?;
//! let source = bound.lookup(location).expect("frontend supplied this location");
//! assert_eq!(source.text(), "40 + 2");
//! assert_eq!(source.start(), SourcePosition { line: 1, byte_column: 1 });
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`SourceRange::text`], [`SourceRange::start`] and [`SourceRange::end`] borrow/query the
//! validated snapshot without allocating previews. [`BoundSources::context`] additionally
//! builds bounded, sanitized structured lines; terminal layout belongs to the caller.
//! The standalone sidecar codec preserves supplied source contents and is separate from tape
//! encoding. Source maps are never attached to [`Program`] or retained by verification/VM runs.

use crate::{disasm::disassemble_function, program::Program, trace::TraceLocation, value::FuncId};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, ops::Range};

/// Index into one map's document snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceId(pub u32);

/// Frontend-provided document label and exact UTF-8 contents. Labels are never opened as paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDocument {
    /// User-facing document name or logical path.
    pub name: String,
    /// Exact snapshot against which byte ranges were produced.
    pub text: String,
}

/// One exact instruction/declaration location mapped to a half-open source byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMapping {
    /// Program-local function and exact source bytecode PC.
    pub location: TraceLocation,
    /// Document snapshot in this map.
    pub document: SourceId,
    /// Half-open UTF-8 byte range; empty ranges represent a source point.
    pub range: Range<usize>,
}

/// Limits on used input bytes/items; decoded program storage and allocator overhead are additional.
#[derive(Clone, Copy, Debug)]
pub struct SourceLimits {
    /// Maximum canonical encoded program identity bytes.
    pub program_bytes: usize,
    /// Maximum total UTF-8 snapshot and label bytes.
    pub source_bytes: usize,
    /// Maximum document snapshots.
    pub documents: usize,
    /// Maximum total line index entries across all documents.
    pub lines: usize,
    /// Maximum explicit instruction mappings.
    pub mappings: usize,
}
impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            program_bytes: 64 * 1024 * 1024,
            source_bytes: 16 * 1024 * 1024,
            documents: 1024,
            lines: 1_000_000,
            mappings: 1_000_000,
        }
    }
}

/// Source data could not be validated or attached. Failed construction exposes no partial map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// An input item/byte budget was exceeded.
    Limit(&'static str),
    /// The standalone source sidecar is malformed or unsupported.
    InvalidEncoding,
    /// Required raw program ranges are malformed, or embedded program bytes cannot be decoded.
    InvalidProgram,
    /// A map belongs to a different canonical encoded program.
    ProgramMismatch,
    /// No instruction/declaration exists at this function/PC.
    InvalidLocation(TraceLocation),
    /// Two mappings were supplied for the same instruction.
    DuplicateLocation(TraceLocation),
    /// A mapping references an absent document.
    InvalidDocument(SourceId),
    /// Range is reversed, outside the snapshot, or splits a UTF-8 code point.
    InvalidRange {
        /// Mapping's instruction.
        location: TraceLocation,
        /// Referenced snapshot.
        document: SourceId,
        /// Supplied half-open byte range.
        range: Range<usize>,
        /// Original snapshot byte length.
        document_bytes: usize,
    },
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(resource) => write!(f, "source map exceeds {resource} limit"),
            Self::InvalidEncoding => f.write_str("invalid or unsupported source sidecar"),
            Self::InvalidProgram => f.write_str("source map program identity cannot be encoded or decoded"),
            Self::ProgramMismatch => f.write_str("source map belongs to a different program; regenerate it from this exact compilation"),
            Self::InvalidLocation(loc) => write!(f, "source mapping has no instruction at f{}:{}", loc.func.0, loc.pc),
            Self::DuplicateLocation(loc) => write!(f, "duplicate source mapping at f{}:{}", loc.func.0, loc.pc),
            Self::InvalidDocument(id) => write!(f, "source mapping references absent document {}", id.0),
            Self::InvalidRange { location, document, range, document_bytes } => write!(f, "source range {}..{} at f{}:{} in document {} ({} bytes) is reversed, outside the snapshot, or not on UTF-8 boundaries", range.start, range.end, location.func.0, location.pc, document.0, document_bytes),
        }
    }
}
impl core::error::Error for SourceError {}

/// Owned optional source sidecar. Use [`Self::bind`] before resolving program-local locations.
#[derive(Clone)]
pub struct SourceMap {
    program: Vec<u8>,
    documents: Vec<SourceDocument>,
    mappings: Vec<SourceMapping>,
    line_starts: Vec<Vec<usize>>,
}
fn identity(program: &Program) -> Result<Vec<u8>, SourceError> {
    // These are the required ranges whose infallible writer has documented panic preconditions.
    for (index, constant) in program.const_pool.iter().enumerate() {
        if matches!(constant, crate::program::ConstEntry::Decimal(_)) {
            let id = u32::try_from(index).map_err(|_| SourceError::InvalidProgram)?;
            program
                .const_decimal(crate::program::ConstId(id))
                .map_err(|_| SourceError::InvalidProgram)?;
        }
    }
    for function in &program.functions {
        function
            .register_types(program)
            .map_err(|_| SourceError::InvalidProgram)?;
        function
            .arg_contracts(program)
            .map_err(|_| SourceError::InvalidProgram)?;
        function
            .ret_contracts(program)
            .map_err(|_| SourceError::InvalidProgram)?;
    }
    for signature in &program.call_sigs {
        program
            .call_sig_arg_contracts(signature)
            .map_err(|_| SourceError::InvalidProgram)?;
        program
            .call_sig_ret_contracts(signature)
            .map_err(|_| SourceError::InvalidProgram)?;
    }
    Ok(program.encode())
}
impl SourceMap {
    /// Copies one full canonical program identity and validates all supplied locations and ranges.
    /// The program may be semantically rejected; mapped functions must still decode.
    pub fn new(
        program: &Program,
        documents: Vec<SourceDocument>,
        mut mappings: Vec<SourceMapping>,
        limits: SourceLimits,
    ) -> Result<Self, SourceError> {
        if documents.len() > limits.documents {
            return Err(SourceError::Limit("document count"));
        }
        if mappings.len() > limits.mappings {
            return Err(SourceError::Limit("mapping count"));
        }
        let source_bytes = documents
            .iter()
            .try_fold(0_usize, |total, d| {
                total
                    .checked_add(d.name.len())
                    .and_then(|n| n.checked_add(d.text.len()))
            })
            .ok_or(SourceError::Limit("source bytes"))?;
        if source_bytes > limits.source_bytes {
            return Err(SourceError::Limit("source bytes"));
        }
        let encoded = identity(program)?;
        if encoded.len() > limits.program_bytes {
            return Err(SourceError::Limit("program identity bytes"));
        }
        mappings.sort_unstable_by_key(|m| (m.location.func.0, m.location.pc));
        let mut previous = None;
        let mut functions = BTreeMap::new();
        for mapping in &mappings {
            if previous == Some(mapping.location) {
                return Err(SourceError::DuplicateLocation(mapping.location));
            }
            previous = Some(mapping.location);
            let source = if let Some(source) = functions.get(&mapping.location.func.0) {
                source
            } else {
                let source = disassemble_function(program, mapping.location.func)
                    .map_err(|_| SourceError::InvalidLocation(mapping.location))?;
                functions.insert(mapping.location.func.0, source);
                &functions[&mapping.location.func.0]
            };
            if source.instruction(mapping.location.pc).is_none() {
                return Err(SourceError::InvalidLocation(mapping.location));
            }
            let document = documents
                .get(mapping.document.0 as usize)
                .ok_or(SourceError::InvalidDocument(mapping.document))?;
            if mapping.range.start > mapping.range.end
                || document.text.get(mapping.range.clone()).is_none()
            {
                return Err(SourceError::InvalidRange {
                    location: mapping.location,
                    document: mapping.document,
                    range: mapping.range.clone(),
                    document_bytes: document.text.len(),
                });
            }
        }
        let line_count = documents
            .iter()
            .try_fold(0_usize, |n, d| {
                n.checked_add(1 + d.text.bytes().filter(|&byte| byte == b'\n').count())
            })
            .ok_or(SourceError::Limit("line indexes"))?;
        if line_count > limits.lines {
            return Err(SourceError::Limit("line indexes"));
        }
        let line_starts = documents
            .iter()
            .map(|d| {
                core::iter::once(0)
                    .chain(d.text.match_indices('\n').map(|(i, _)| i + 1))
                    .collect()
            })
            .collect();
        Ok(Self {
            program: encoded,
            documents,
            mappings,
            line_starts,
        })
    }

    /// Validates the full canonical program identity once; lookup thereafter does not encode again.
    pub fn bind<'a>(&'a self, program: &Program) -> Result<BoundSources<'a>, SourceError> {
        if identity(program)? != self.program {
            return Err(SourceError::ProgramMismatch);
        }
        Ok(BoundSources { map: self })
    }

    /// Encodes a standalone source sidecar, including exact source snapshots and program identity.
    /// This is an explicit export of supplied source text, separate from the tape wire format.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"ETSM\x01".to_vec();
        fn number(out: &mut Vec<u8>, value: usize) {
            out.extend_from_slice(&(value as u64).to_le_bytes());
        }
        fn bytes(out: &mut Vec<u8>, value: &[u8]) {
            number(out, value.len());
            out.extend_from_slice(value);
        }
        bytes(&mut out, &self.program);
        number(&mut out, self.documents.len());
        for document in &self.documents {
            bytes(&mut out, document.name.as_bytes());
            bytes(&mut out, document.text.as_bytes());
        }
        number(&mut out, self.mappings.len());
        for mapping in &self.mappings {
            out.extend_from_slice(&mapping.location.func.0.to_le_bytes());
            out.extend_from_slice(&mapping.location.pc.to_le_bytes());
            out.extend_from_slice(&mapping.document.0.to_le_bytes());
            number(&mut out, mapping.range.start);
            number(&mut out, mapping.range.end);
        }
        out
    }

    /// Decodes and validates a source sidecar under explicit byte/item budgets before reserving.
    pub fn decode(bytes: &[u8], limits: SourceLimits) -> Result<Self, SourceError> {
        struct Reader<'a>(&'a [u8]);
        impl<'a> Reader<'a> {
            fn take(&mut self, n: usize) -> Result<&'a [u8], SourceError> {
                let value = self.0.get(..n).ok_or(SourceError::InvalidEncoding)?;
                self.0 = &self.0[n..];
                Ok(value)
            }
            fn number(&mut self) -> Result<usize, SourceError> {
                usize::try_from(u64::from_le_bytes(
                    self.take(8)?.try_into().expect("eight bytes"),
                ))
                .map_err(|_| SourceError::InvalidEncoding)
            }
            fn small(&mut self) -> Result<u32, SourceError> {
                Ok(u32::from_le_bytes(
                    self.take(4)?.try_into().expect("four bytes"),
                ))
            }
            fn bytes(&mut self, max: usize) -> Result<&'a [u8], SourceError> {
                let n = self.number()?;
                if n > max {
                    return Err(SourceError::Limit("encoded bytes"));
                }
                self.take(n)
            }
        }
        let mut r = Reader(bytes);
        if r.take(5)? != b"ETSM\x01" {
            return Err(SourceError::InvalidEncoding);
        }
        let program = Program::decode(r.bytes(limits.program_bytes)?)
            .map_err(|_| SourceError::InvalidProgram)?;
        let count = r.number()?;
        if count > limits.documents {
            return Err(SourceError::Limit("document count"));
        }
        if count > r.0.len() / 16 {
            return Err(SourceError::InvalidEncoding);
        }
        let mut documents = Vec::with_capacity(count);
        let mut remaining = limits.source_bytes;
        for _ in 0..count {
            let name = r.bytes(remaining)?;
            remaining -= name.len();
            let text = r.bytes(remaining)?;
            remaining -= text.len();
            documents.push(SourceDocument {
                name: String::from_utf8(name.to_vec()).map_err(|_| SourceError::InvalidEncoding)?,
                text: String::from_utf8(text.to_vec()).map_err(|_| SourceError::InvalidEncoding)?,
            });
        }
        let count = r.number()?;
        if count > limits.mappings {
            return Err(SourceError::Limit("mapping count"));
        }
        if count > r.0.len() / 28 {
            return Err(SourceError::InvalidEncoding);
        }
        let mut mappings = Vec::with_capacity(count);
        for _ in 0..count {
            mappings.push(SourceMapping {
                location: TraceLocation {
                    func: FuncId(r.small()?),
                    pc: r.small()?,
                },
                document: SourceId(r.small()?),
                range: r.number()?..r.number()?,
            });
        }
        if !r.0.is_empty() {
            return Err(SourceError::InvalidEncoding);
        }
        Self::new(&program, documents, mappings, limits)
    }
}

/// A map checked against an exact program. Cheap to copy and reusable across headless queries and UI selections.
#[derive(Clone, Copy, Debug)]
pub struct BoundSources<'a> {
    map: &'a SourceMap,
}
/// Borrowed exact mapped source range.
#[derive(Clone)]
pub struct SourceRange<'a> {
    document: &'a SourceDocument,
    range: Range<usize>,
    lines: &'a [usize],
}

/// One-based coordinates in the original UTF-8 snapshot, independent of terminal layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    /// One-based source line.
    pub line: usize,
    /// One-based UTF-8 byte column, not a character, UTF-16 or display-cell column.
    pub byte_column: usize,
}

impl<'a> SourceRange<'a> {
    /// Borrows the original document snapshot and unescaped label.
    #[must_use]
    pub fn document(&self) -> &'a SourceDocument {
        self.document
    }

    /// Returns the validated half-open UTF-8 byte range.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Borrows the exact selected text without copying or escaping it.
    #[must_use]
    pub fn text(&self) -> &'a str {
        &self.document.text[self.range.clone()]
    }

    /// Returns the start coordinates without constructing preview lines.
    #[must_use]
    pub fn start(&self) -> SourcePosition {
        self.position(self.range.start)
    }

    /// Returns the exclusive end coordinates without constructing preview lines.
    #[must_use]
    pub fn end(&self) -> SourcePosition {
        self.position(self.range.end)
    }

    fn position(&self, offset: usize) -> SourcePosition {
        let line = self
            .lines
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        SourcePosition {
            line: line + 1,
            byte_column: offset - self.lines[line] + 1,
        }
    }
}
/// Sanitized bounded source line. The selected segments correspond to the exact original range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLine {
    /// One-based line number.
    pub number: usize,
    /// Sanitized text before the selected range.
    pub before: String,
    /// Sanitized selected text (empty for a source point).
    pub selected: String,
    /// Sanitized text after the selected range.
    pub after: String,
    /// Text before this viewport has been omitted.
    pub clipped_left: bool,
    /// Text after this viewport has been omitted.
    pub clipped_right: bool,
}
/// Bounded source context, retaining exact byte coordinates separately from display text.
#[derive(Clone)]
pub struct SourceContext<'a> {
    /// Original unescaped document label; escape controls before displaying it.
    pub name: &'a str,
    /// Exact half-open byte range in the original UTF-8 snapshot.
    pub range: Range<usize>,
    /// Start coordinates in the original snapshot.
    pub start: SourcePosition,
    /// Exclusive end coordinates in the original snapshot.
    pub end: SourcePosition,
    /// At most the requested/clamped number of lines.
    pub lines: Vec<SourceLine>,
    /// The source range continues beyond the displayed lines.
    pub continued: bool,
}
fn escaped(ch: char) -> String {
    if ch.is_control()
        || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    {
        ch.escape_default().to_string()
    } else {
        ch.to_string()
    }
}
fn safe_label(label: &str) -> String {
    let mut output = String::new();
    let mut count = 0;
    for ch in label.chars() {
        let escaped = escaped(ch);
        let width = escaped.chars().count();
        if count + width > 120 {
            output.push('…');
            break;
        }
        output.push_str(&escaped);
        count += width;
    }
    output
}
impl fmt::Debug for SourceMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceMap")
            .field("program_bytes", &self.program.len())
            .field("documents", &self.documents.len())
            .field("mappings", &self.mappings.len())
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SourceRange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceRange")
            .field("document", &safe_label(&self.document.name))
            .field("document_bytes", &self.document.text.len())
            .field("range", &self.range)
            .finish_non_exhaustive()
    }
}
impl fmt::Debug for SourceContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceContext")
            .field("name", &safe_label(self.name))
            .field("range", &self.range)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("lines", &self.lines)
            .field("continued", &self.continued)
            .finish()
    }
}
impl<'a> BoundSources<'a> {
    /// Resolves only an explicitly supplied exact location. Missing mappings never inherit spans.
    pub fn lookup(self, location: TraceLocation) -> Option<SourceRange<'a>> {
        let index = self
            .map
            .mappings
            .binary_search_by_key(&(location.func.0, location.pc), |m| {
                (m.location.func.0, m.location.pc)
            })
            .ok()?;
        let mapping = &self.map.mappings[index];
        Some(SourceRange {
            document: &self.map.documents[mapping.document.0 as usize],
            range: mapping.range.clone(),
            lines: &self.map.line_starts[mapping.document.0 as usize],
        })
    }

    /// Selects context around the start of the exact range. Limits clamp to 1..=12 lines and
    /// 16..=240 sanitized Unicode scalars per line; neither is a terminal-cell measurement.
    pub fn context(
        self,
        location: TraceLocation,
        max_lines: usize,
        max_columns: usize,
    ) -> Option<SourceContext<'a>> {
        let source = self.lookup(location)?;
        let max_lines = max_lines.clamp(1, 12);
        let max_columns = max_columns.clamp(16, 240);
        let start = source.start();
        let end = source.end();
        let first = (start.line - 1).saturating_sub(usize::from(max_lines > 1));
        let last = (first + max_lines).min(source.lines.len());
        let mut lines = Vec::new();
        for line in first..last {
            let begin = source.lines[line];
            let finish = source
                .lines
                .get(line + 1)
                .map_or(source.document.text.len(), |&n| n - 1);
            let text = &source.document.text[begin..finish];
            let selected_start = source.range.start.saturating_sub(begin).min(text.len());
            let selected_end = source.range.end.saturating_sub(begin).min(text.len());
            let focus = if line + 1 == start.line {
                selected_start
            } else {
                0
            };
            // Walk backwards only far enough to retain a bounded lookback. Do not scan or
            // materialize the potentially enormous prefix before a far-right source range.
            let mut window_start = focus;
            let mut lookback = 0;
            for (offset, ch) in text[..focus].char_indices().rev() {
                let width = escaped(ch).chars().count();
                if lookback + width > max_columns / 3 {
                    break;
                }
                lookback += width;
                window_start = offset;
            }
            let mut cells = Vec::with_capacity(max_columns + 1);
            'characters: for (relative, ch) in text[window_start..].char_indices() {
                let offset = window_start + relative;
                let zone = if offset < selected_start {
                    0
                } else if offset < selected_end {
                    1
                } else {
                    2
                };
                for ch in escaped(ch).chars() {
                    cells.push((ch, zone));
                    if cells.len() > max_columns {
                        break 'characters;
                    }
                }
            }
            let clipped_right = cells.len() > max_columns;
            cells.truncate(max_columns);
            let mut before = String::new();
            let mut selected = String::new();
            let mut after = String::new();
            for (ch, zone) in cells {
                match zone {
                    0 => before.push(ch),
                    1 => selected.push(ch),
                    _ => after.push(ch),
                }
            }
            lines.push(SourceLine {
                number: line + 1,
                before,
                selected,
                after,
                clipped_left: window_start > 0,
                clipped_right,
            });
        }
        Some(SourceContext {
            name: &source.document.name,
            range: source.range.clone(),
            start,
            end,
            lines,
            continued: end.line > last,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asm::{Asm, FunctionSig, ProgramBuilder},
        program::ValueType,
    };
    use alloc::{format, vec};
    fn fixture(value: i64) -> (Program, TraceLocation) {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.const_i64(1, value).ret(0, &[1]);
        let mut b = ProgramBuilder::new();
        let func = b
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        (b.build_checked().unwrap(), TraceLocation { func, pc: 0 })
    }
    fn source(
        program: &Program,
        location: TraceLocation,
        text: &str,
        range: Range<usize>,
    ) -> Result<SourceMap, SourceError> {
        SourceMap::new(
            program,
            vec![SourceDocument {
                name: "example.expr".into(),
                text: text.into(),
            }],
            vec![SourceMapping {
                location,
                document: SourceId(0),
                range,
            }],
            SourceLimits::default(),
        )
    }
    #[test]
    fn exact_identity_includes_constants_and_survives_codec() {
        let (program, location) = fixture(42);
        let map = source(&program, location, "answer = 42\n", 9..11).unwrap();
        let encoded = map.encode();
        let decoded = SourceMap::decode(&encoded, SourceLimits::default()).unwrap();
        assert_eq!(decoded.encode(), encoded);
        assert_eq!(
            decoded
                .bind(&program)
                .unwrap()
                .lookup(location)
                .unwrap()
                .range,
            9..11
        );
        let (stale, _) = fixture(43);
        assert!(matches!(
            map.bind(&stale),
            Err(SourceError::ProgramMismatch)
        ));
        let mut changed_pool = program.clone();
        changed_pool
            .const_pool
            .push(crate::program::ConstEntry::I64(9));
        assert!(matches!(
            map.bind(&changed_pool),
            Err(SourceError::ProgramMismatch)
        ));
        let clone = Program::decode(&program.encode()).unwrap();
        assert!(map.bind(&clone).is_ok());
    }
    #[test]
    fn invalid_ranges_locations_and_ambiguity_fail_before_binding() {
        let (p, loc) = fixture(42);
        for range in [1..2, 0..4, Range { start: 2, end: 1 }] {
            assert!(matches!(
                source(&p, loc, "é!", range),
                Err(SourceError::InvalidRange { .. })
            ));
        }
        let map = source(&p, loc, "é!", 0..2).unwrap();
        let bound = map.bind(&p).unwrap();
        assert_eq!(bound.context(loc, 1, 20).unwrap().lines[0].selected, "é");
        assert!(matches!(
            source(&p, TraceLocation { pc: 1, ..loc }, "x", 0..1),
            Err(SourceError::InvalidLocation(_))
        ));
        let duplicate = SourceMapping {
            location: loc,
            document: SourceId(0),
            range: 0..1,
        };
        assert!(matches!(
            SourceMap::new(
                &p,
                vec![SourceDocument {
                    name: "x".into(),
                    text: "x".into()
                }],
                vec![duplicate.clone(), duplicate],
                SourceLimits::default()
            ),
            Err(SourceError::DuplicateLocation(_))
        ));
        assert!(matches!(
            SourceMap::new(
                &p,
                vec![],
                vec![SourceMapping {
                    location: loc,
                    document: SourceId(9),
                    range: 0..0
                }],
                SourceLimits::default()
            ),
            Err(SourceError::InvalidDocument(SourceId(9)))
        ));
    }
    #[test]
    fn missing_mappings_are_not_inferred_from_other_locations() {
        let (p, loc) = fixture(42);
        let map = source(&p, loc, "42", 0..2).unwrap();
        let bound = map.bind(&p).unwrap();
        let other = TraceLocation {
            pc: u32::MAX,
            ..loc
        };
        assert!(bound.lookup(other).is_none());
        assert!(bound.context(other, 4, 80).is_none());
    }
    #[test]
    fn context_is_bounded_and_controls_are_visible() {
        let (p, loc) = fixture(42);
        let text = format!(
            "before\n{}é\u{1b}[31m\t\u{202e}Z\nafter\n",
            "x".repeat(100_000)
        );
        let start = text.find('é').unwrap();
        let map = source(&p, loc, &text, start..text.find('Z').unwrap() + 1).unwrap();
        let bound = map.bind(&p).unwrap();
        let context = bound.context(loc, 1, 48).unwrap();
        assert_eq!(context.lines.len(), 1);
        assert_eq!(context.lines[0].number, 2);
        assert!(context.lines[0].clipped_left);
        assert!(
            context.lines[0].before.chars().count()
                + context.lines[0].selected.chars().count()
                + context.lines[0].after.chars().count()
                <= 48
        );
        let context = bound.context(loc, 1, 80).unwrap();
        let shown = context
            .lines
            .iter()
            .map(|line| format!("{}{}{}", line.before, line.selected, line.after))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!shown.contains('\u{1b}'));
        assert!(!shown.contains('\t'));
        assert!(!shown.contains('\u{202e}'));
        assert!(shown.contains("\\u{1b}") || shown.contains("\\x1b"));
    }
    #[test]
    fn point_ranges_and_multiline_endpoints_keep_exact_coordinates() {
        let (p, loc) = fixture(42);
        let map = source(&p, loc, "ab\ncd\n", 4..4).unwrap();
        let context = map.bind(&p).unwrap().context(loc, 1, 40).unwrap();
        assert_eq!(
            context.start,
            SourcePosition {
                line: 2,
                byte_column: 2
            }
        );
        assert_eq!(context.lines[0].before, "c");
        assert_eq!(context.lines[0].after, "d");
        let eof = source(&p, loc, "ab\n", 3..3).unwrap();
        assert_eq!(
            eof.bind(&p).unwrap().context(loc, 1, 40).unwrap().start,
            SourcePosition {
                line: 2,
                byte_column: 1
            }
        );
        let map = source(&p, loc, "a\nb\nc\nd", 0..7).unwrap();
        assert!(map.bind(&p).unwrap().context(loc, 2, 40).unwrap().continued);
    }
    #[test]
    fn untrusted_codec_checks_prefixes_counts_and_budgets() {
        let (p, loc) = fixture(42);
        let map = source(&p, loc, "42", 0..2).unwrap();
        let bytes = map.encode();
        for len in 0..bytes.len() {
            assert!(
                SourceMap::decode(&bytes[..len], SourceLimits::default()).is_err(),
                "prefix {len}"
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(SourceMap::decode(&extra, SourceLimits::default()).is_err());
        assert!(matches!(
            SourceMap::decode(
                &bytes,
                SourceLimits {
                    program_bytes: 1,
                    ..SourceLimits::default()
                }
            ),
            Err(SourceError::Limit(_))
        ));
        assert!(matches!(
            SourceMap::decode(
                &bytes,
                SourceLimits {
                    source_bytes: 1,
                    ..SourceLimits::default()
                }
            ),
            Err(SourceError::Limit(_))
        ));
        assert!(matches!(
            SourceMap::decode(
                &bytes,
                SourceLimits {
                    mappings: 0,
                    ..SourceLimits::default()
                }
            ),
            Err(SourceError::Limit(_))
        ));
        let mut counts = b"ETSM\x01".to_vec();
        counts.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(SourceMap::decode(&counts, SourceLimits::default()).is_err());
    }
    #[test]
    fn malformed_raw_program_is_an_error_instead_of_writer_panic() {
        let (p, loc) = fixture(42);
        let map = source(&p, loc, "42", 0..2).unwrap();
        let mut malformed = p.clone();
        malformed.functions[0].register_types.len = u32::MAX;
        assert!(matches!(
            map.bind(&malformed),
            Err(SourceError::InvalidProgram)
        ));
        let mut malformed = p;
        malformed.call_sigs.push(crate::program::CallSigEntry {
            args: crate::program::ByteRange {
                offset: u32::MAX,
                len: 1,
            },
            rets: crate::program::ByteRange { offset: 0, len: 0 },
        });
        assert!(matches!(
            map.bind(&malformed),
            Err(SourceError::InvalidProgram)
        ));
    }

    #[test]
    fn malformed_decimal_storage_is_rejected_for_source_creation_and_binding() {
        use crate::program::{ByteRange, ConstEntry};

        let (mut program, loc) = fixture(42);
        program.const_bytes_data.resize(17, 0);
        program
            .const_pool
            .push(ConstEntry::Decimal(ByteRange { offset: 0, len: 17 }));
        let map = source(&program, loc, "42", 0..2).unwrap();
        for range in [
            ByteRange { offset: 0, len: 16 },
            ByteRange { offset: 0, len: 18 },
            ByteRange { offset: 1, len: 17 },
            ByteRange {
                offset: u32::MAX,
                len: 17,
            },
        ] {
            let mut malformed = program.clone();
            let last = malformed.const_pool.last_mut().unwrap();
            *last = ConstEntry::Decimal(range);
            assert!(matches!(
                SourceMap::new(&malformed, vec![], vec![], SourceLimits::default()),
                Err(SourceError::InvalidProgram)
            ));
            assert!(matches!(
                map.bind(&malformed),
                Err(SourceError::InvalidProgram)
            ));
        }
    }
    #[test]
    fn repeated_far_right_windows_keep_utf8_boundaries_and_bounded_debug() {
        let (p, loc) = fixture(42);
        let text = format!("{}é界TAIL", "wide界".repeat(100_000));
        let start = text.len() - "界TAIL".len();
        let map = source(&p, loc, &text, start..start + "界".len()).unwrap();
        let bound = map.bind(&p).unwrap();
        for _ in 0..64 {
            let context = bound.context(loc, 1, 16).unwrap();
            assert_eq!(context.lines[0].selected, "界");
            assert!(context.lines[0].clipped_left);
            assert!(context.lines[0].before.chars().count() <= 5);
        }
        let debug = format!("{:?}", bound.lookup(loc).unwrap());
        assert!(debug.len() < 240);
        assert!(!debug.contains("wide界wide界"));
    }
}
