//! Grouping of push-parsed NAL units into _access units_.
//!
//! An H.264 access unit consists of the VCL NAL units making up one primary
//! coded picture (all the slices of one frame or one field) together with the
//! non-VCL NAL units that the standard requires to appear with it (access unit
//! delimiter, parameter sets, SEI, prefix NAL units, etc.).  Muxing an H.264
//! elementary stream into MP4 samples, or timing and dropping whole images,
//! needs access-unit boundaries rather than the per-NAL delivery of
//! [`crate::push::NalAccumulator`].
//!
//! [`AccessUnitGrouper`] implements the boundary detection rules of
//! Rec. ITU-T H.264 section 7.4.1.2.4, using the "first VCL NAL unit of a
//! primary coded picture" tests of section 7.4.1.2.3.  Non-VCL NAL units that
//! appear before the first VCL NAL unit are held back until the picture they
//! belong to is known, so they are never split into an access unit of their
//! own.  The grouper sits on top of the existing push interface (it implements
//! [`AccumulatedNalHandler`]) and only ever examines *complete* NAL units, so
//! its output does not depend on how the input bytes are chunked.
//!
//! # Example
//!
//! ```
//! use h264_reader::annexb::AnnexBReader;
//! use h264_reader::nal::{Nal, RefNal, UnitType};
//! use h264_reader::push::access_unit::{AccessUnitGrouper, AccessUnitHandler};
//!
//! // A handler that records, for each access unit, the NAL unit types it contains.
//! #[derive(Default)]
//! struct Rec { groups: Vec<Vec<UnitType>> }
//! impl AccessUnitHandler for Rec {
//!     fn start(&mut self) { self.groups.push(Vec::new()); }
//!     fn nal(&mut self, nal: RefNal<'_>) {
//!         self.groups.last_mut().unwrap().push(nal.header().unwrap().nal_unit_type());
//!     }
//! }
//!
//! let mut reader = AnnexBReader::accumulate(AccessUnitGrouper::new(Rec::default()));
//! // AUD + IDR slice, then another AUD + non-IDR slice: two access units.
//! reader.push(&[
//!     0, 0, 0, 1, 0x09, 0x10,             // access unit delimiter
//!     0, 0, 0, 1, 0x65, 0x88, 0x80, 0x40, // IDR slice
//!     0, 0, 0, 1, 0x09, 0x10,             // access unit delimiter
//!     0, 0, 0, 1, 0x41, 0x9A,             // non-IDR slice
//! ]);
//! reader.reset();
//! reader.nal_handler_mut().reset();
//!
//! let groups = &reader.into_nal_handler().into_handler().groups;
//! assert_eq!(groups.len(), 2);
//! assert_eq!(groups[0], [UnitType::AccessUnitDelimiter, UnitType::SliceLayerWithoutPartitioningIdr]);
//! assert_eq!(groups[1], [UnitType::AccessUnitDelimiter, UnitType::SliceLayerWithoutPartitioningNonIdr]);
//! ```

use crate::nal::pps::PicParameterSet;
use crate::nal::slice::{Field, FieldPic, PicOrderCountLsb, SliceHeader};
use crate::nal::sps::{PicOrderCntType, SeqParameterSet};
use crate::nal::subset_sps::SubsetSps;
use crate::nal::{
    parse_nal_header_extension, Nal, NalHeader, NalHeaderError, NalHeaderExtension, RefNal,
    UnitType,
};
use crate::push::{AccumulatedNalHandler, NalInterest};
use crate::rbsp::BitRead;
use crate::Context;
use std::io::Read;

/// Why a NAL unit could not be parsed, as reported by
/// [`AccessUnitHandler::unparseable`].
///
/// The offending NAL unit is still delivered through
/// [`AccessUnitHandler::nal`] (with `start`/`end` wrapping) and remains in its
/// access unit; this error only signals that the syntax needed for boundary
/// detection or context tracking could not be read.  Unparseable NAL units
/// never disturb the grouping of the NAL units that follow them.
#[derive(Debug)]
pub enum NalParseError {
    /// The NAL header byte had `forbidden_zero_bit` set (or was otherwise
    /// invalid); see [`NalHeaderError`].
    NalHeader(NalHeaderError),
    /// A sequence parameter set NAL unit failed to parse.
    Sps(crate::nal::sps::SpsError),
    /// A picture parameter set NAL unit failed to parse.
    Pps(crate::nal::pps::PpsError),
    /// A subset sequence parameter set NAL unit failed to parse.
    SubsetSps(crate::nal::sps::SpsError),
    /// A VCL NAL unit's slice header failed to parse.
    SliceHeader(crate::nal::slice::SliceHeaderError),
    /// The 3-byte extended NAL header of a type 14/20 NAL unit could not be
    /// read (e.g. the NAL unit was truncated).
    HeaderExtension(std::io::Error),
}
impl std::fmt::Display for NalParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NalParseError::NalHeader(e) => write!(f, "invalid NAL header: {:?}", e),
            NalParseError::Sps(e) => write!(f, "SPS parse error: {:?}", e),
            NalParseError::Pps(e) => write!(f, "PPS parse error: {:?}", e),
            NalParseError::SubsetSps(e) => write!(f, "subset SPS parse error: {:?}", e),
            NalParseError::SliceHeader(e) => write!(f, "slice header parse error: {:?}", e),
            NalParseError::HeaderExtension(e) => {
                write!(f, "NAL header extension read error: {}", e)
            }
        }
    }
}
impl std::error::Error for NalParseError {}

/// Receives the NAL units of each access unit in stream order.
///
/// Every call sequence is `start`, zero or more `nal`/`unparseable` calls, then
/// `end`.  Each [`RefNal`] borrows data owned by the [`AccessUnitGrouper`] and
/// is only valid for the duration of the callback; copy the bytes if they must
/// outlive it.
///
/// All methods have default no-op implementations, so handlers override only
/// the events they care about.
pub trait AccessUnitHandler {
    /// Called at the beginning of a new access unit, before any of its NAL
    /// units are delivered.
    fn start(&mut self) {}
    /// Called with one complete NAL unit belonging to the current access unit.
    fn nal(&mut self, _nal: RefNal<'_>) {}
    /// Called when a NAL unit could not be parsed.
    ///
    /// The default implementation forwards to [`nal`](Self::nal), so handlers
    /// that don't care about parse errors receive the bytes like any other NAL
    /// unit.  Override to record the [`NalParseError`].  The NAL unit remains
    /// part of its access unit.
    fn unparseable(&mut self, nal: RefNal<'_>, _error: &NalParseError) {
        self.nal(nal);
    }
    /// Called after the last NAL unit of an access unit has been delivered.
    fn end(&mut self) {}
}

// Closure adapter, mirroring AccumulatedNalHandler.
impl<F: FnMut(RefNal<'_>)> AccessUnitHandler for F {
    fn nal(&mut self, nal: RefNal<'_>) {
        (self)(nal)
    }
}

/// Which family of VCL NAL unit a [`VclInfo`] describes.
///
/// The families are kept distinct because an access unit contains only one of
/// them (a base-view primary picture, an auxiliary picture, or a particular
/// MVC/SVC view component).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum VclKind {
    /// NAL unit types 1-5: primary coded picture slices (including data
    /// partitions).
    Primary,
    /// NAL unit type 19: auxiliary coded picture slice.
    Aux,
    /// NAL unit types 20/21: coded slice extension (MVC/SVC view component).
    Extension,
}

/// Picture-order-count snapshot used by section 7.4.1.2.3 conditions (h)/(i).
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum PocSnapshot {
    /// `pic_order_cnt_type == 0`.
    TypeZero {
        pic_order_cnt_lsb: u32,
        delta_pic_order_cnt_bottom: i32,
    },
    /// `pic_order_cnt_type == 1`.
    TypeOne { delta_pic_order_cnt: [i32; 2] },
    /// `pic_order_cnt_type == 2`; the test does not apply.
    TypeTwo,
}

/// The fields of a successfully-parsed VCL NAL unit that section 7.4.1.2.3
/// compares between two consecutive VCL NAL units.
#[derive(Clone, Debug)]
struct VclInfo {
    kind: VclKind,
    nal_ref_idc: u8,
    pps_id: u8,
    frame_num: u16,
    field_pic: bool,
    bottom_field: bool,
    idr: bool,
    idr_pic_id: Option<u32>,
    poc: PocSnapshot,
    redundant_pic_cnt: u32,
    /// MVC view identifier (NAL unit types 20/21 with an MVC extension),
    /// `None` otherwise.
    view_id: Option<u16>,
}

/// State carried over from the previous VCL NAL unit.
enum PrevVcl {
    /// The previous VCL NAL unit had a parseable slice header.
    Full(VclInfo),
    /// The previous VCL NAL unit could not be parsed; only the
    /// `first_mb_in_slice == 0` heuristic remains available.
    Partial {
        kind: VclKind,
        first_mb_zero: Option<bool>,
    },
}

/// Push-parser layer that groups complete NAL units into access units.
///
/// Construct with [`AccessUnitGrouper::new`], feed NAL units through the
/// [`AccumulatedNalHandler`] interface (typically wrapped in a
/// [`crate::push::NalAccumulator`] driven by an
/// [`crate::annexb::AnnexBReader`] or an AVC-length-prefixed reader), and call
/// [`AccessUnitGrouper::reset`] at end of stream to flush the final access
/// unit.
///
/// Parameter-set NAL units are parsed as they arrive and, on success, stored in
/// an internal [`Context`] so that later slice headers can be resolved.  The
/// context is also accessible through [`context`](Self::context) and
/// [`context_mut`](Self::context_mut).
pub struct AccessUnitGrouper<H: AccessUnitHandler> {
    handler: H,
    context: Context,

    /// Complete NAL units of the access unit currently being delivered.
    ///
    /// Only needed so that [`RefNal`]s handed to the handler have a stable,
    /// owning allocation; each entry is one whole NAL unit including its
    /// header byte and emulation-prevention bytes.
    current: Vec<Vec<u8>>,
    /// Whether an access unit is currently open (`start` called, `end` not
    /// yet).
    au_open: bool,
    /// Whether the open access unit already contains a VCL NAL unit.
    has_vcl: bool,

    /// Non-VCL NAL units (parameter sets, SEI, prefix NAL units, ...) that
    /// precede the first VCL NAL unit of the next picture.  They are held
    /// until it is known whether they belong to the current access unit
    /// (followed by end-of-sequence/filler) or the next one (followed by a
    /// VCL NAL unit).  Each entry carries the parse error (if any) to report
    /// when the NAL unit is delivered.
    pending: Vec<PendingNal>,

    prev_vcl: Option<PrevVcl>,
}

/// A complete NAL unit held back pending a picture boundary, together with any
/// error encountered while parsing it.
struct PendingNal {
    bytes: Vec<u8>,
    error: Option<NalParseError>,
}

impl<H: AccessUnitHandler> AccessUnitGrouper<H> {
    /// Creates a new grouper which delivers access units to `handler`, using a
    /// fresh, empty parser [`Context`].
    pub fn new(handler: H) -> Self {
        Self::with_context(handler, Context::new())
    }

    /// Creates a new grouper which reuses an existing parser [`Context`] (e.g.
    /// one already populated from an AVC decoder configuration record).
    pub fn with_context(handler: H, context: Context) -> Self {
        Self {
            handler,
            context,
            current: Vec::new(),
            au_open: false,
            has_vcl: false,
            pending: Vec::new(),
            prev_vcl: None,
        }
    }

    /// Gets a reference to the handler.
    pub fn handler(&self) -> &H {
        &self.handler
    }
    /// Gets a mutable reference to the handler.
    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }
    /// Unwraps this grouper, returning the inner handler.
    pub fn into_handler(self) -> H {
        self.handler
    }

    /// Gets a reference to the parser context holding the parameter sets seen
    /// so far.
    pub fn context(&self) -> &Context {
        &self.context
    }
    /// Gets a mutable reference to the parser context.
    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Flushes the final access unit at end of stream.
    ///
    /// After this call the grouper is ready to parse a new, independent
    /// stream (carried-over VCL comparison state is cleared); parameter sets
    /// in the [`Context`] are retained.  Calling this when no access unit is
    /// open has no effect.
    pub fn reset(&mut self) {
        if self.pending.is_empty() {
            // No buffered non-VCL NAL units: flush an open access unit if any,
            // otherwise (e.g. already closed by end_of_seq) do nothing.
            self.close_current();
            self.prev_vcl = None;
            return;
        }
        // Pending non-VCL NAL units precede the next picture; with no further
        // picture arriving they form an access unit of their own, distinct
        // from the picture already closed.
        self.close_current();
        self.ensure_open();
        self.flush_pending_into_current();
        self.close_current();
        self.prev_vcl = None;
    }

    fn ensure_open(&mut self) {
        if !self.au_open {
            self.current.clear();
            self.handler.start();
            self.au_open = true;
            self.has_vcl = false;
        }
    }

    fn close_current(&mut self) {
        if self.au_open {
            self.handler.end();
            self.current.clear();
            self.au_open = false;
            self.has_vcl = false;
        }
    }

    /// Emits all pending non-VCL NAL units into the current (open) access unit.
    fn flush_pending_into_current(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for pending_nal in pending {
            self.emit(pending_nal.bytes, pending_nal.error.as_ref());
        }
    }

    /// Delivers one complete NAL unit to the handler as part of the current
    /// (open) access unit, retaining the bytes for the lifetime of the
    /// callback.
    fn emit(&mut self, buf: Vec<u8>, error: Option<&NalParseError>) {
        debug_assert!(self.au_open);
        self.current.push(buf);
        let nal = RefNal::new(self.current.last().unwrap(), &[], true);
        match error {
            Some(e) => self.handler.unparseable(nal, e),
            None => self.handler.nal(nal),
        }
    }

    /// Handles one complete NAL unit.
    fn handle_complete(&mut self, buf: Vec<u8>) {
        let header = match NalHeader::new(buf[0]) {
            Ok(h) => h,
            Err(e) => {
                // Forbidden zero bit etc.: report but keep grouping intact.
                self.ensure_open();
                self.emit(buf, Some(&NalParseError::NalHeader(e)));
                return;
            }
        };
        match header.nal_unit_type() {
            UnitType::AccessUnitDelimiter => {
                // AUD is always the first NAL unit of an access unit.  Close
                // any picture already complete, then open the new access unit
                // and emit the AUD before the pending non-VCL NAL units that
                // precede the same picture.
                if self.au_open {
                    self.close_current();
                }
                self.ensure_open();
                self.emit(buf, None);
                self.flush_pending_into_current();
            }

            // Non-VCL NAL units that section 7.4.1.2.4 places before the first
            // VCL NAL unit of an access unit.  If a picture is already open
            // they belong to the *next* access unit and must be held back.
            UnitType::SEI
            | UnitType::SeqParameterSet
            | UnitType::PicParameterSet
            | UnitType::SeqParameterSetExtension
            | UnitType::PrefixNALUnit
            | UnitType::SubsetSeqParameterSet
            | UnitType::DepthParameterSet => {
                let error = self.parse_non_vcl(header, &buf);
                if self.has_vcl {
                    // Defer until the next picture (or end of stream); the
                    // parse error is reported when it is actually delivered.
                    self.pending.push(PendingNal { bytes: buf, error });
                } else {
                    // Before any picture: either starts a new access unit, or
                    // joins the one already being assembled.
                    self.ensure_open();
                    self.emit(buf, error.as_ref());
                }
            }

            // end_of_seq / end_of_stream close the access unit of the picture
            // they follow; any pending parameter sets/SEI belong to that same
            // access unit rather than a later picture.
            UnitType::EndOfSeq | UnitType::EndOfStream => {
                self.ensure_open();
                self.flush_pending_into_current();
                self.emit(buf, None);
                self.close_current();
            }

            // Filler data follows the last VCL NAL unit of the current
            // picture; pending non-VCL NAL units come before that picture's
            // VCL NAL units, so they join the current access unit first.
            UnitType::FillerData => {
                self.ensure_open();
                self.flush_pending_into_current();
                self.emit(buf, None);
            }

            // VCL NAL units.
            UnitType::SliceLayerWithoutPartitioningNonIdr
            | UnitType::SliceDataPartitionALayer
            | UnitType::SliceDataPartitionBLayer
            | UnitType::SliceDataPartitionCLayer
            | UnitType::SliceLayerWithoutPartitioningIdr
            | UnitType::SliceLayerWithoutPartitioningAux
            | UnitType::SliceExtension
            | UnitType::SliceExtensionViewComponent => {
                self.handle_vcl(header, buf);
            }

            // Unspecified / reserved NAL units have no defined ordering; pass
            // them through without changing any boundary state.
            UnitType::Unspecified(_) | UnitType::Reserved(_) => {
                self.ensure_open();
                self.emit(buf, None);
            }
        }
    }

    /// Parses parameter-set NAL units, updating the context on success and
    /// returning the error to report on failure.
    fn parse_non_vcl(&mut self, header: NalHeader, buf: &[u8]) -> Option<NalParseError> {
        let nal = RefNal::new(buf, &[], true);
        match header.nal_unit_type() {
            UnitType::SeqParameterSet => match SeqParameterSet::from_bits(nal.rbsp_bits()) {
                Ok(sps) => {
                    self.context.put_seq_param_set(sps);
                    None
                }
                Err(e) => Some(NalParseError::Sps(e)),
            },
            UnitType::PicParameterSet => {
                match PicParameterSet::from_bits(&self.context, nal.rbsp_bits()) {
                    Ok(pps) => {
                        self.context.put_pic_param_set(pps);
                        None
                    }
                    Err(e) => Some(NalParseError::Pps(e)),
                }
            }
            UnitType::SubsetSeqParameterSet => match SubsetSps::from_bits(nal.rbsp_bits()) {
                Ok(subset) => {
                    self.context.put_subset_seq_param_set(subset);
                    None
                }
                Err(e) => Some(NalParseError::SubsetSps(e)),
            },
            _ => None,
        }
    }

    fn handle_vcl(&mut self, header: NalHeader, buf: Vec<u8>) {
        let kind = vcl_kind(header.nal_unit_type());

        // Parse first (section 7.4.1.2.3 compares the current slice header
        // against the previous one), then decide the boundary.
        let nal = RefNal::new(&buf, &[], true);
        let parsed = self.try_parse_vcl(header, kind, &nal);
        let (mut start_new, parse_error) = match parsed {
            Ok(Some(info)) => {
                let start_new = self.boundary_from_parsed(kind, &info);
                self.prev_vcl = Some(PrevVcl::Full(info));
                (start_new, None)
            }
            Ok(None) => {
                // Data partition B/C: no slice header; it always belongs to
                // the picture of the preceding partition A.
                (false, None)
            }
            Err(e) => {
                let first_mb_zero = read_first_mb(&nal);
                let start_new = self.boundary_from_heuristic(kind, first_mb_zero);
                self.prev_vcl = Some(PrevVcl::Partial {
                    kind,
                    first_mb_zero,
                });
                (start_new, Some(e))
            }
        };
        // An access unit already opened by pre-picture non-VCL NAL units (AUD,
        // parameter sets, SEI, ...) is this picture's access unit; the VCL
        // comparison only separates pictures once VCL NAL units are appended
        // directly.
        if self.au_open && !self.has_vcl {
            start_new = false;
        }
        self.begin_vcl_access_unit(start_new);
        self.has_vcl = true;
        self.emit(buf, parse_error.as_ref());
    }

    /// Opens (or continues) the access unit holding a VCL NAL unit, attaching
    /// any pending non-VCL NAL units which precede the picture.
    fn begin_vcl_access_unit(&mut self, start_new: bool) {
        if start_new && self.au_open {
            self.close_current();
        }
        self.ensure_open();
        self.flush_pending_into_current();
    }

    /// Boundary decision when both the previous and current VCL NAL units were
    /// parsed successfully: section 7.4.1.2.3 conditions (a)-(j).
    ///
    /// This is only reached once the current access unit already holds a
    /// picture (or no access unit is open); pre-picture non-VCL NAL units are
    /// handled before this point.
    fn boundary_from_parsed(&self, kind: VclKind, cur: &VclInfo) -> bool {
        let prev = match &self.prev_vcl {
            None => {
                // First VCL NAL unit of the stream.
                return !self.au_open;
            }
            Some(PrevVcl::Full(prev)) => prev,
            Some(PrevVcl::Partial {
                kind: prev_kind,
                first_mb_zero,
            }) => {
                // The previous slice header was unparseable, so the full
                // section-7.4.1.2.3 comparison cannot be made.  Fall back to
                // the conservative first_mb_in_slice rule.
                return if *prev_kind != kind {
                    true
                } else {
                    matches!(first_mb_zero, Some(true))
                };
            }
        };

        // Different picture families always start a new picture.
        if prev.kind != kind {
            return true;
        }
        // (a) IDR picture boundary.
        if prev.idr || cur.idr {
            if prev.idr != cur.idr {
                return true;
            }
            if prev.idr_pic_id != cur.idr_pic_id {
                return true;
            }
        }
        // (b) frame_num differs.
        if prev.frame_num != cur.frame_num {
            return true;
        }
        // (c) different picture parameter set.
        if prev.pps_id != cur.pps_id {
            return true;
        }
        // (d) field_pic_flag differs.
        if prev.field_pic != cur.field_pic {
            return true;
        }
        // (e)/(f) bottom_field_flag differs between field pictures.
        if prev.bottom_field != cur.bottom_field {
            return true;
        }
        // (g) nal_ref_idc differs where one side is zero.
        if (prev.nal_ref_idc == 0) != (cur.nal_ref_idc == 0) {
            return true;
        }
        // (h) pic_order_cnt_type == 0 comparison.
        if prev.poc != cur.poc {
            return true;
        }
        // (j) MVC view components with different view_id belong to different
        // pictures (each view component is its own access unit).
        if prev.view_id != cur.view_id {
            return true;
        }
        // redundant_pic_cnt differs (redundant coded pictures are distinct).
        if prev.redundant_pic_cnt != cur.redundant_pic_cnt {
            return true;
        }
        false
    }

    /// Boundary decision when the current slice header could not be parsed.
    ///
    /// Only the `first_mb_in_slice == 0` empirical rule remains available.  To
    /// avoid splitting a multi-slice picture whose headers cannot be parsed,
    /// the rule is applied conservatively: a new picture is started only when
    /// the rule positively identifies a first slice.
    fn boundary_from_heuristic(&self, kind: VclKind, first_mb_zero: Option<bool>) -> bool {
        match &self.prev_vcl {
            // First VCL NAL unit of the stream.
            None => !self.au_open,
            Some(PrevVcl::Full(prev)) => {
                if prev.kind != kind {
                    return true;
                }
                matches!(first_mb_zero, Some(true))
            }
            Some(PrevVcl::Partial {
                kind: prev_kind,
                first_mb_zero: prev_first_mb,
            }) => {
                if *prev_kind != kind {
                    return true;
                }
                matches!(prev_first_mb, Some(true))
            }
        }
    }

    /// Parses a VCL NAL unit's slice header when one is present.
    ///
    /// Returns `Ok(None)` for data partition B/C NAL units, which contain no
    /// slice header.
    fn try_parse_vcl(
        &self,
        header: NalHeader,
        kind: VclKind,
        nal: &RefNal<'_>,
    ) -> Result<Option<VclInfo>, NalParseError> {
        if matches!(
            header.nal_unit_type(),
            UnitType::SliceDataPartitionBLayer | UnitType::SliceDataPartitionCLayer
        ) {
            return Ok(None);
        }
        let ext = if matches!(
            header.nal_unit_type(),
            UnitType::SliceExtension | UnitType::SliceExtensionViewComponent
        ) {
            Some(
                parse_nal_header_extension(nal)
                    .map_err(NalParseError::HeaderExtension)?
                    .0,
            )
        } else {
            None
        };
        let (slice, sps, pps) =
            SliceHeader::from_bits(&self.context, &mut nal.rbsp_bits(), header, ext.as_ref())
                .map_err(NalParseError::SliceHeader)?;
        let view_id = match &ext {
            Some(NalHeaderExtension::Mvc(mvc)) => Some(mvc.view_id()),
            _ => None,
        };
        let idr = matches!(
            header.nal_unit_type(),
            UnitType::SliceLayerWithoutPartitioningIdr
        ) || matches!(&ext, Some(NalHeaderExtension::Mvc(mvc)) if !mvc.non_idr_flag());
        let field_pic = matches!(slice.field_pic, FieldPic::Field(_));
        let bottom_field = matches!(slice.field_pic, FieldPic::Field(Field::Bottom));
        let poc = match (&sps.pic_order_cnt, &slice.pic_order_cnt_lsb) {
            (PicOrderCntType::TypeZero { .. }, Some(v)) => match v {
                PicOrderCountLsb::Frame(pic_order_cnt_lsb) => PocSnapshot::TypeZero {
                    pic_order_cnt_lsb: *pic_order_cnt_lsb,
                    delta_pic_order_cnt_bottom: 0,
                },
                PicOrderCountLsb::FieldsAbsolute {
                    pic_order_cnt_lsb,
                    delta_pic_order_cnt_bottom,
                } => PocSnapshot::TypeZero {
                    pic_order_cnt_lsb: *pic_order_cnt_lsb,
                    delta_pic_order_cnt_bottom: *delta_pic_order_cnt_bottom,
                },
                PicOrderCountLsb::FieldsDelta(_) => PocSnapshot::TypeTwo,
            },
            (PicOrderCntType::TypeOne { .. }, Some(v)) => match v {
                PicOrderCountLsb::FieldsDelta(d) => PocSnapshot::TypeOne {
                    delta_pic_order_cnt: *d,
                },
                _ => PocSnapshot::TypeOne {
                    delta_pic_order_cnt: [0, 0],
                },
            },
            _ => PocSnapshot::TypeTwo,
        };
        Ok(Some(VclInfo {
            kind,
            nal_ref_idc: header.nal_ref_idc(),
            pps_id: pps.pic_parameter_set_id.id(),
            frame_num: slice.frame_num,
            field_pic,
            bottom_field,
            idr,
            idr_pic_id: slice.idr_pic_id,
            poc,
            redundant_pic_cnt: slice.redundant_pic_cnt.unwrap_or(0),
            view_id,
        }))
    }
}

/// Heuristic used when only the current NAL unit's `first_mb_in_slice` is
/// available: a zero value marks the first slice of a new picture.
fn vcl_kind(t: UnitType) -> VclKind {
    match t {
        UnitType::SliceLayerWithoutPartitioningAux => VclKind::Aux,
        UnitType::SliceExtension | UnitType::SliceExtensionViewComponent => VclKind::Extension,
        _ => VclKind::Primary,
    }
}

/// Reads just the `first_mb_in_slice` unsigned Exp-Golomb value from a VCL
/// NAL unit's RBSP, returning `None` if it cannot be read.
fn read_first_mb(nal: &RefNal<'_>) -> Option<bool> {
    nal.rbsp_bits()
        .read_ue("first_mb_in_slice")
        .ok()
        .map(|v| v == 0)
}

impl<H: AccessUnitHandler> AccumulatedNalHandler for AccessUnitGrouper<H> {
    fn nal(&mut self, nal: RefNal<'_>) -> NalInterest {
        if !nal.is_complete() {
            // Let the NalAccumulator keep buffering; grouping only ever sees
            // whole NAL units so that chunking cannot affect the result.
            return NalInterest::Buffer;
        }
        let mut buf = Vec::new();
        nal.reader()
            .read_to_end(&mut buf)
            .expect("writing to Vec<u8> cannot fail");
        self.handle_complete(buf);
        NalInterest::Ignore
    }
}

impl<H: AccessUnitHandler + std::fmt::Debug> std::fmt::Debug for AccessUnitGrouper<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessUnitGrouper")
            .field("handler", &self.handler)
            .field("au_open", &self.au_open)
            .field("has_vcl", &self.has_vcl)
            .field("pending_nal_units", &self.pending.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the delivered access units as vectors of (nal type, unparseable?).
    #[derive(Default)]
    struct Recorder {
        units: Vec<Vec<(UnitType, bool)>>,
    }
    impl AccessUnitHandler for Recorder {
        fn start(&mut self) {
            self.units.push(Vec::new());
        }
        fn nal(&mut self, nal: RefNal<'_>) {
            let ty = nal
                .header()
                .map(|h| h.nal_unit_type())
                .unwrap_or(UnitType::Unspecified(0));
            self.units.last_mut().unwrap().push((ty, false));
        }
        fn unparseable(&mut self, nal: RefNal<'_>, _error: &NalParseError) {
            let ty = nal
                .header()
                .map(|h| h.nal_unit_type())
                .unwrap_or(UnitType::Unspecified(0));
            self.units.last_mut().unwrap().push((ty, true));
        }
    }

    // Minimal NAL unit builders.  The ordering tests use AUD and non-VCL NAL
    // units, which need no slice-header/parameter-set parsing.
    fn aud() -> Vec<u8> {
        vec![0x09, 0x10] // primary_pic_type=0 + trailing bits
    }
    fn sei() -> Vec<u8> {
        vec![0x06, 0x80]
    }
    fn filler() -> Vec<u8> {
        vec![0x0C, 0xFF]
    }
    fn end_of_seq() -> Vec<u8> {
        vec![0x0A, 0x00]
    }
    fn idr() -> Vec<u8> {
        // Type 5, nal_ref_idc=3, RBSP beginning with ue(0)=1 bit.
        vec![0x65, 0x88, 0x80, 0x40]
    }
    fn non_idr() -> Vec<u8> {
        // Type 1, nal_ref_idc=2.
        vec![0x41, 0x9A]
    }

    fn grouper() -> AccessUnitGrouper<Recorder> {
        AccessUnitGrouper::new(Recorder::default())
    }

    fn push(g: &mut AccessUnitGrouper<Recorder>, nal: &[u8]) {
        g.nal(RefNal::new(nal, &[], true));
    }

    fn types(group: &[(UnitType, bool)]) -> Vec<UnitType> {
        group.iter().map(|(t, _)| *t).collect()
    }

    #[test]
    fn aud_delimits_access_units() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &idr());
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        assert_eq!(g.handler().units.len(), 2);
        assert_eq!(
            types(&g.handler().units[0]),
            vec![
                UnitType::AccessUnitDelimiter,
                UnitType::SliceLayerWithoutPartitioningIdr
            ]
        );
        assert_eq!(
            types(&g.handler().units[1]),
            vec![
                UnitType::AccessUnitDelimiter,
                UnitType::SliceLayerWithoutPartitioningNonIdr
            ]
        );
    }

    #[test]
    fn parameter_sets_before_picture_join_that_picture() {
        let mut g = grouper();
        // SEI appears after the previous picture's slices but before the next
        // VCL NAL unit; it must join the next access unit, not stand alone.
        push(&mut g, &aud());
        push(&mut g, &idr());
        push(&mut g, &sei());
        push(&mut g, &non_idr());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 2);
        assert_eq!(
            types(&groups[1]),
            vec![UnitType::SEI, UnitType::SliceLayerWithoutPartitioningNonIdr]
        );
    }

    #[test]
    fn end_of_seq_closes_picture_and_keeps_pending_with_it() {
        let mut g = grouper();
        push(&mut g, &idr());
        push(&mut g, &sei()); // pending: before next picture
        push(&mut g, &end_of_seq());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 1);
        assert_eq!(
            types(&groups[0]),
            vec![
                UnitType::SliceLayerWithoutPartitioningIdr,
                UnitType::SEI,
                UnitType::EndOfSeq,
            ]
        );
    }

    #[test]
    fn filler_stays_with_current_picture() {
        let mut g = grouper();
        push(&mut g, &idr());
        push(&mut g, &filler());
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 2);
        assert_eq!(
            types(&groups[0]),
            vec![
                UnitType::SliceLayerWithoutPartitioningIdr,
                UnitType::FillerData,
            ]
        );
    }

    #[test]
    fn final_access_unit_delivered_on_reset() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &idr());
        // No explicit end; reset must flush.
        g.reset();
        assert_eq!(g.handler().units.len(), 1);
        // Idempotent.
        g.reset();
        assert_eq!(g.handler().units.len(), 1);
    }

    #[test]
    fn empty_stream_produces_no_access_units() {
        let mut g = grouper();
        g.reset();
        assert!(g.handler().units.is_empty());
    }

    #[test]
    fn reset_allows_reuse_for_independent_stream() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &idr());
        g.reset();
        assert_eq!(g.handler().units.len(), 1);

        // A subsequent, independent stream starts grouping afresh; the
        // carried-over VCL state must not merge its first picture with the
        // previous one.
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        assert_eq!(g.handler().units.len(), 2);
        assert_eq!(
            types(&g.handler().units[1]),
            vec![
                UnitType::AccessUnitDelimiter,
                UnitType::SliceLayerWithoutPartitioningNonIdr
            ]
        );
    }

    #[test]
    fn pending_non_vcl_at_end_forms_access_unit() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &idr());
        push(&mut g, &sei()); // never followed by another picture
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 2);
        assert_eq!(types(&groups[1]), vec![UnitType::SEI]);
    }

    #[test]
    fn invalid_header_reported_without_breaking_grouping() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &[0xFF, 0x11]); // forbidden_zero_bit set
        push(&mut g, &idr());
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 2);
        assert!(groups[0].iter().any(|(_, bad)| *bad));
    }

    #[test]
    fn deferred_param_set_error_reported_with_next_picture() {
        let mut g = grouper();
        push(&mut g, &aud());
        push(&mut g, &idr());
        // A truncated SPS after the picture's VCL is deferred to the next AU;
        // its parse error must surface there, not split or corrupt grouping.
        push(&mut g, &[0x67]); // SPS with no RBSP: unparseable
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 2);
        // The bad SPS joined the second access unit (with its AUD and slice).
        assert_eq!(
            types(&groups[1]),
            vec![
                UnitType::AccessUnitDelimiter,
                UnitType::SeqParameterSet,
                UnitType::SliceLayerWithoutPartitioningNonIdr,
            ]
        );
        let bad_sps = groups[1]
            .iter()
            .any(|(t, bad)| *t == UnitType::SeqParameterSet && *bad);
        assert!(bad_sps);
    }

    #[test]
    fn unparseable_vcl_does_not_break_later_boundaries() {
        let mut g = grouper();
        // Each picture is delimited by an AUD.  No SPS/PPS are present in the
        // context, so every slice header fails to parse, but the AUD
        // boundaries must still be honoured and each failure reported.
        push(&mut g, &aud());
        push(&mut g, &idr());
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        push(&mut g, &aud());
        push(&mut g, &non_idr());
        g.reset();
        let groups = &g.handler().units;
        assert_eq!(groups.len(), 3);
        // Every slice was flagged as unparseable but still delivered in the
        // access unit its AUD opened.
        assert!(groups[0]
            .iter()
            .any(|(t, bad)| *t == UnitType::SliceLayerWithoutPartitioningIdr && *bad));
        for group in &groups[1..] {
            assert!(group
                .iter()
                .any(|(t, bad)| *t == UnitType::SliceLayerWithoutPartitioningNonIdr && *bad));
        }
    }

    #[test]
    fn unparseable_vcl_without_aud_uses_first_mb_heuristic() {
        // No AUDs: boundary detection falls back to first_mb_in_slice == 0.
        let mut g = grouper();
        push(&mut g, &idr()); // first_mb == 0: opens picture 1
        push(&mut g, &non_idr()); // first_mb == 0: heuristic starts picture 2
        g.reset();
        assert_eq!(g.handler().units.len(), 2);
    }

    #[test]
    fn chunking_does_not_change_grouping() {
        // Drive through NalAccumulator + AnnexBReader with arbitrary fragment
        // boundaries and compare against whole-stream delivery.
        fn run(chunk: usize) -> Vec<Vec<UnitType>> {
            let stream: Vec<u8> = [
                &[0, 0, 0, 1][..],
                &aud(),
                &[0, 0, 0, 1][..],
                &idr(),
                &[0, 0, 0, 1][..],
                &aud(),
                &[0, 0, 0, 1][..],
                &non_idr(),
            ]
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect();

            let mut reader = crate::annexb::AnnexBReader::accumulate(AccessUnitGrouper::new(
                Recorder::default(),
            ));
            for part in stream.chunks(chunk.max(1)) {
                reader.push(part);
            }
            reader.reset();
            reader.nal_handler_mut().reset();
            reader
                .into_nal_handler()
                .into_handler()
                .units
                .into_iter()
                .map(|u| u.into_iter().map(|(t, _)| t).collect())
                .collect()
        }
        let whole = run(1024);
        for chunk in 1..=7 {
            assert_eq!(run(chunk), whole, "chunk size {}", chunk);
        }
        assert_eq!(whole.len(), 2);
    }

    #[test]
    fn first_mb_reader() {
        let bytes = idr();
        let nal = RefNal::new(&bytes, &[], true);
        assert_eq!(read_first_mb(&nal), Some(true));
    }

    /// End-to-end test over a minimal, fully-valid Baseline stream whose slice
    /// headers all parse.  Exercises the section-7.4.1.2.3 comparison path
    /// (rather than the AUD/first_mb fallbacks): SPS+PPS must join the first
    /// picture, multi-slice pictures stay together, and a changed frame_num
    /// opens a new access unit.
    #[test]
    fn parses_valid_stream_into_access_units() {
        // Generated NAL units (Baseline, 16x16, pic_order_cnt_type 2):
        let sps = [0x67u8, 0x42, 0x00, 0x0a, 0xdc, 0x94, 0x40];
        let pps = [0x68u8, 0xce, 0x38, 0x80];
        // Each slice NAL has header bytes followed by two slice-data bytes
        // (the header parser only requires that slice data exists).
        let idr_f0 = [0x65u8, 0xb8, 0x47, 0xff, 0xfe]; // IDR,     frame_num 0, first_mb 0
        let sl_f1_mb0 = [0x41u8, 0xb8, 0x9f, 0xff, 0xf8]; // non-IDR, frame_num 1, first_mb 0
        let sl_f1_mb1 = [0x41u8, 0x4e, 0x27, 0xff, 0xfe]; // same picture,        first_mb 1
        let sl_f2_mb0 = [0x41u8, 0xb9, 0x1f, 0xff, 0xf8]; // non-IDR, frame_num 2, first_mb 0

        let mut g = grouper();
        for nal in [
            &sps[..],
            &pps[..],
            &idr_f0[..],
            &sl_f1_mb0[..],
            &sl_f1_mb1[..],
            &sl_f2_mb0[..],
        ] {
            push(&mut g, nal);
        }
        g.reset();

        // No NAL unit should be unparseable.
        for group in &g.handler().units {
            assert!(
                group.iter().all(|(_, bad)| !bad),
                "unexpected parse failure"
            );
        }
        let groups: Vec<Vec<UnitType>> = g
            .handler()
            .units
            .iter()
            .map(|u| u.iter().map(|(t, _)| *t).collect())
            .collect();
        assert_eq!(
            groups,
            vec![
                // Parameter sets precede and join the first picture.
                vec![
                    UnitType::SeqParameterSet,
                    UnitType::PicParameterSet,
                    UnitType::SliceLayerWithoutPartitioningIdr,
                ],
                // Both slices of frame_num 1 stay in one access unit.
                vec![
                    UnitType::SliceLayerWithoutPartitioningNonIdr,
                    UnitType::SliceLayerWithoutPartitioningNonIdr,
                ],
                // frame_num 2 starts a new access unit.
                vec![UnitType::SliceLayerWithoutPartitioningNonIdr],
            ]
        );

        // The parsed parameter sets were stored in the context.
        assert_eq!(g.context().sps().count(), 1);
        assert_eq!(g.context().pps().count(), 1);
    }

    /// Same valid stream delivered through AnnexBReader+NalAccumulator in
    /// arbitrary chunk sizes must produce identical grouping.
    #[test]
    fn valid_stream_grouping_is_chunk_independent() {
        let nals: [&[u8]; 6] = [
            &[0x67u8, 0x42, 0x00, 0x0a, 0xdc, 0x94, 0x40],
            &[0x68u8, 0xce, 0x38, 0x80],
            &[0x65u8, 0xb8, 0x47, 0xff, 0xfe],
            &[0x41u8, 0xb8, 0x9f, 0xff, 0xf8],
            &[0x41u8, 0x4e, 0x27, 0xff, 0xfe],
            &[0x41u8, 0xb9, 0x1f, 0xff, 0xf8],
        ];
        let mut stream = Vec::new();
        for nal in nals {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(nal);
        }
        fn run(stream: &[u8], chunk: usize) -> Vec<Vec<UnitType>> {
            let mut reader = crate::annexb::AnnexBReader::accumulate(AccessUnitGrouper::new(
                Recorder::default(),
            ));
            for part in stream.chunks(chunk.max(1)) {
                reader.push(part);
            }
            reader.reset();
            reader.nal_handler_mut().reset();
            reader
                .into_nal_handler()
                .into_handler()
                .units
                .into_iter()
                .map(|u| u.into_iter().map(|(t, _)| t).collect())
                .collect()
        }
        let whole = run(&stream, 1024);
        assert_eq!(whole.len(), 3);
        for chunk in 1..=5 {
            assert_eq!(run(&stream, chunk), whole, "chunk size {}", chunk);
        }
    }

    #[test]
    fn section_7_4_1_2_3_conditions() {
        // Build a base VclInfo and check each condition triggers a boundary.
        let base = VclInfo {
            kind: VclKind::Primary,
            nal_ref_idc: 2,
            pps_id: 0,
            frame_num: 7,
            field_pic: false,
            bottom_field: false,
            idr: false,
            idr_pic_id: None,
            poc: PocSnapshot::TypeZero {
                pic_order_cnt_lsb: 10,
                delta_pic_order_cnt_bottom: 0,
            },
            redundant_pic_cnt: 0,
            view_id: None,
        };
        let mut g = grouper();

        // Same info => same picture.
        g.with_prev(PrevVcl::Full(base.clone()));
        assert!(!g.boundary_from_parsed(VclKind::Primary, &base));

        let mut different = base.clone();
        different.frame_num = 8;
        assert!(g.boundary_from_parsed(VclKind::Primary, &different));

        let mut different = base.clone();
        different.pps_id = 1;
        assert!(g.boundary_from_parsed(VclKind::Primary, &different));

        let mut different = base.clone();
        different.field_pic = true;
        different.bottom_field = true;
        assert!(g.boundary_from_parsed(VclKind::Primary, &different));

        let mut different = base.clone();
        different.nal_ref_idc = 0;
        assert!(g.boundary_from_parsed(VclKind::Primary, &different));

        let mut different = base.clone();
        different.poc = PocSnapshot::TypeZero {
            pic_order_cnt_lsb: 11,
            delta_pic_order_cnt_bottom: 0,
        };
        assert!(g.boundary_from_parsed(VclKind::Primary, &different));

        // IDR transition.
        let mut idr = base.clone();
        idr.idr = true;
        idr.idr_pic_id = Some(0);
        assert!(g.boundary_from_parsed(VclKind::Primary, &idr));

        // Different picture family.
        assert!(g.boundary_from_parsed(VclKind::Aux, &base));
    }

    impl<H: AccessUnitHandler> AccessUnitGrouper<H> {
        fn with_prev(&mut self, prev: PrevVcl) {
            self.prev_vcl = Some(prev);
        }
    }
}
