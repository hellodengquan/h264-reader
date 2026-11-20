//! Derivation of picture order count values (spec section 8.2.1).
//!
//! The slice header only carries syntax elements local to the current picture (e.g.
//! `frame_num`, `pic_order_cnt_lsb` or `delta_pic_order_cnt`); on their own these do
//! not identify where the picture sits in display order. [`PicOrderCounter`] keeps the
//! state that the spec mandates be tracked across pictures and applies the decoding
//! process for all three `pic_order_cnt_type` modes, producing absolute top- and
//! bottom-field picture order counts for every picture it is given.
//!
//! A new counter has no derivation origin until it sees an IDR picture; pictures
//! presented before that point (or after coding parameters have changed in an
//! incompatible way) produce explicit errors rather than guesses. Because the state
//! depends only on the sequence of slice headers and their parameter sets, splitting a
//! long stream into segments and feeding each segment into the same counter — starting
//! each segment at any IDR picture — yields identical results to feeding the whole
//! stream at once.
//!
//! Only the first slice of each primary coded picture should be passed in; slices with
//! `first_mb_in_slice != 0`, and redundant coded pictures, are rejected. When
//! `separate_colour_plane_flag` is set each colour plane needs its own counter.
//!
//! # Example
//!
//! ```no_run
//! use h264_reader::nal::slice::poc::{PicOrder, PicOrderCounter};
//!
//! let mut poc = PicOrderCounter::new();
//! // For each primary coded picture, after parsing its first slice header together with
//! // the SPS/PPS it activates:
//! # let (header, sps, pps) = todo!();
//! match poc.record_picture(&header, &sps, &pps) {
//!     Ok(PicOrder::Frame {
//!         top_field_pic_order_cnt,
//!         bottom_field_pic_order_cnt,
//!     }) => {
//!         // A frame-coded picture gives the order count of both fields.
//!         let _ = (top_field_pic_order_cnt, bottom_field_pic_order_cnt);
//!     }
//!     Ok(PicOrder::Field { field, pic_order_cnt }) => {
//!         // A field-coded picture gives the count of its single field.
//!         let _ = (field, pic_order_cnt);
//!     }
//!     Err(e) => {
//!         // No IDR origin yet, or incompatible coding parameters.
//!         eprintln!("picture order count unavailable: {e}");
//!     }
//! }
//! ```

use super::{
    DecRefPicMarking, FieldPic, MemoryManagementControlOperation, PicOrderCountLsb, SliceHeader,
};
use crate::nal::pps::PicParameterSet;
use crate::nal::sps::{FrameMbsFlags, PicOrderCntType, SeqParameterSet};

/// Which of the two fields a field-coded picture represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrder {
    Top,
    Bottom,
}

/// The picture order count values derived for one primary coded picture.
///
/// Values are the absolute `TopFieldPicOrderCnt` / `BottomFieldPicOrderCnt` variables
/// from the spec (not the modular `pic_order_cnt_lsb` syntax element). A frame-coded
/// picture always carries both field counts; a field-coded picture carries the count of
/// the single field it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PicOrder {
    /// A frame-coded picture, giving the order counts of both its fields.
    Frame {
        top_field_pic_order_cnt: i64,
        bottom_field_pic_order_cnt: i64,
    },
    /// A field-coded picture, giving the order count of its single field.
    Field {
        field: FieldOrder,
        pic_order_cnt: i64,
    },
}

impl PicOrder {
    /// The picture order count of the top field (for a frame, the top-field count; for a
    /// top field, its own count; for a bottom field, the bottom-field count).
    pub fn top_field_pic_order_cnt(&self) -> Option<i64> {
        match self {
            PicOrder::Frame {
                top_field_pic_order_cnt,
                ..
            } => Some(*top_field_pic_order_cnt),
            PicOrder::Field {
                field: FieldOrder::Top,
                pic_order_cnt,
            } => Some(*pic_order_cnt),
            PicOrder::Field {
                field: FieldOrder::Bottom,
                ..
            } => None,
        }
    }

    /// The picture order count of the bottom field, if the picture contains one.
    pub fn bottom_field_pic_order_cnt(&self) -> Option<i64> {
        match self {
            PicOrder::Frame {
                bottom_field_pic_order_cnt,
                ..
            } => Some(*bottom_field_pic_order_cnt),
            PicOrder::Field {
                field: FieldOrder::Bottom,
                pic_order_cnt,
            } => Some(*pic_order_cnt),
            PicOrder::Field {
                field: FieldOrder::Top,
                ..
            } => None,
        }
    }
}

/// Errors that can occur while deriving picture order counts.
#[derive(Debug)]
pub enum PicOrderCntError {
    /// No IDR picture has been seen yet, so the spec variables needed to derive an
    /// absolute order count have no defined value.
    NoReferencePoint,
    /// The picture's sequence parameter set changes the picture-order-related coding
    /// parameters relative to the IDR picture that established the current state.
    /// A new IDR picture is required to re-establish a derivation origin.
    IncompatibleSequenceParameters(&'static str),
    /// The slice is not the first slice of its picture (`first_mb_in_slice != 0`).
    /// Order counts are derived per picture; call once with the first slice.
    NotFirstSliceInPicture,
    /// The slice belongs to a redundant coded picture, whose order count the spec does
    /// not define through this process.
    RedundantCodedPicture,
    /// A bottom field arrived without the preceding top field whose order count its
    /// derivation requires.
    MissingPreviousTopField,
    /// The stream contains a frame-number gap while
    /// `num_ref_frames_in_pic_order_cnt_cycle` is zero (type 1), so the gap cannot be
    /// accounted for.
    InvalidPicOrderCntCycle,
    /// Intermediate values exceeded the representable range during derivation.
    IntegerOverflow,
}

impl std::fmt::Display for PicOrderCntError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PicOrderCntError::NoReferencePoint => f.write_str(
                "cannot derive picture order count before an IDR picture establishes the origin",
            ),
            PicOrderCntError::IncompatibleSequenceParameters(detail) => write!(
                f,
                "picture order count coding parameters changed incompatibly: {detail}"
            ),
            PicOrderCntError::NotFirstSliceInPicture => f.write_str(
                "picture order count must be derived from the first slice (first_mb_in_slice == 0)",
            ),
            PicOrderCntError::RedundantCodedPicture => {
                f.write_str("redundant coded pictures have no picture order count derivation")
            }
            PicOrderCntError::MissingPreviousTopField => f.write_str(
                "bottom field requires the picture order count of a preceding top field",
            ),
            PicOrderCntError::InvalidPicOrderCntCycle => f.write_str(
                "frame-num gap with num_ref_frames_in_pic_order_cnt_cycle == 0 is unrepresentable",
            ),
            PicOrderCntError::IntegerOverflow => {
                f.write_str("integer overflow while deriving picture order count")
            }
        }
    }
}

impl std::error::Error for PicOrderCntError {}

/// Snapshot of the SPS fields the derivation state is bound to. A change in any of
/// these invalidates the state established at the previous IDR.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PocParamKey {
    log2_max_frame_num: u8,
    pic_order_cnt: PicOrderCntType,
    fields_supported: bool,
}

impl PocParamKey {
    fn from_sps(sps: &SeqParameterSet) -> PocParamKey {
        PocParamKey {
            log2_max_frame_num: sps.log2_max_frame_num(),
            pic_order_cnt: sps.pic_order_cnt.clone(),
            fields_supported: matches!(sps.frame_mbs_flags, FrameMbsFlags::Fields { .. }),
        }
    }
}

/// Position within the `offset_for_ref_frame[]` cycle (type 1 only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CycleState {
    /// `deltaPicOrderCntCycleCnt`
    index: usize,
}

/// Stateful picture order count deriver. See the [module docs](self).
#[derive(Debug, Default)]
pub struct PicOrderCounter {
    key: Option<PocParamKey>,

    // Previous reference picture identification, in the "doubled" convention used by the
    // spec for field pictures (`2 * frame_num + bottom_field_flag`); frame pictures use
    // the plain frame_num.
    prev_ref_pic_num: u32,
    have_prev_ref: bool,

    // Type 0 state (8.2.1.1).
    prev_pic_order_cnt_msb: i64,
    prev_pic_order_cnt_lsb: i64,

    // Type 1 / 2 frame_num offset state (8.2.1.2.1).
    prev_frame_num_offset: i64,
    cycle: CycleState,

    // Most recently derived top-field count of a reference picture, plus the doubled
    // frame-number value of the picture that produced it.
    prev_top_field_pic_order_cnt: i64,
    prev_top_pic_num: u32,
    have_prev_top: bool,
}

impl PicOrderCounter {
    /// Creates a counter with no derivation origin. The first successful call must be
    /// given an IDR picture.
    pub fn new() -> Self {
        Self::default()
    }

    /// Derives the display order count(s) of the picture containing the given slice
    /// header, updating the cross-picture state.
    ///
    /// Call this exactly once per primary coded picture, passing its first slice
    /// (`first_mb_in_slice == 0`), together with the SPS and PPS activated by that
    /// slice header.
    pub fn record_picture(
        &mut self,
        header: &SliceHeader,
        sps: &SeqParameterSet,
        pps: &PicParameterSet,
    ) -> Result<PicOrder, PicOrderCntError> {
        if header.first_mb_in_slice != 0 {
            return Err(PicOrderCntError::NotFirstSliceInPicture);
        }
        if header.redundant_pic_cnt.unwrap_or(0) != 0 {
            return Err(PicOrderCntError::RedundantCodedPicture);
        }

        let key = PocParamKey::from_sps(sps);

        let is_idr = header.idr_pic_id.is_some();
        let is_mmco_reset = matches!(
            &header.dec_ref_pic_marking,
            Some(DecRefPicMarking::Adaptive(ops))
                if ops.iter().any(|op| matches!(
                    op,
                    MemoryManagementControlOperation::AllRefPicturesUnused
                ))
        );
        let origin_reset = is_idr || is_mmco_reset;
        if self.key.is_none() && !origin_reset {
            return Err(PicOrderCntError::NoReferencePoint);
        }
        // A new IDR/MMCO-5 origin may legitimately carry changed coding parameters;
        // outside a reset the parameter key must stay compatible with the active state.
        if !origin_reset {
            if let Some(previous) = &self.key {
                if previous != &key {
                    return Err(PicOrderCntError::IncompatibleSequenceParameters(
                        "log2_max_frame_num, pic_order_cnt_type parameters, or field coding support changed",
                    ));
                }
            }
        }
        self.key = Some(key);

        let is_reference = header.nal_ref_idc != 0;
        let is_bottom_field = matches!(header.field_pic, FieldPic::Field(super::Field::Bottom));
        let is_field = !matches!(header.field_pic, FieldPic::Frame);
        // Doubled frame-number value identifying this specific field/frame, matching the
        // spec's PrevFrameNum/PrevFieldFrameNum comparison variables.
        let current_pic_num = if is_field {
            header.frame_num as u32 * 2 + u32::from(is_bottom_field)
        } else {
            header.frame_num as u32
        };

        let result = match &sps.pic_order_cnt {
            PicOrderCntType::TypeZero {
                log2_max_pic_order_cnt_lsb_minus4,
            } => {
                let max_pic_order_cnt_lsb: i64 = 1i64
                    .checked_shl(u32::from(*log2_max_pic_order_cnt_lsb_minus4) + 4)
                    .ok_or(PicOrderCntError::IntegerOverflow)?;
                self.record_type_zero(
                    header,
                    current_pic_num,
                    max_pic_order_cnt_lsb,
                    is_field,
                    is_bottom_field,
                    origin_reset,
                    is_reference,
                )?
            }
            PicOrderCntType::TypeOne {
                delta_pic_order_always_zero_flag: _,
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offsets_for_ref_frame,
            } => {
                let (d0, d1) = match header.pic_order_cnt_lsb {
                    Some(PicOrderCountLsb::FieldsDelta(d)) => (d[0], d[1]),
                    // delta_pic_order_always_zero_flag produces FieldsDelta([0, 0]);
                    // anything else would be a parser mismatch.
                    _ => (0, 0),
                };
                let frame_num_offset = if is_reference {
                    // Reference picture: consume ref-frame positions, committing state.
                    self.derive_frame_num_offset(
                        sps,
                        current_pic_num,
                        is_field,
                        origin_reset,
                        Some(offsets_for_ref_frame),
                    )?
                } else {
                    // Non-reference picture: derive a temporary offset against the last
                    // reference picture without disturbing stored cycle/offset state.
                    self.derive_non_ref_frame_num_offset(
                        sps,
                        current_pic_num,
                        is_field,
                        Some(offsets_for_ref_frame),
                    )?
                };
                self.record_type_one(
                    header,
                    pps,
                    current_pic_num,
                    is_field,
                    is_bottom_field,
                    origin_reset,
                    is_reference,
                    *offset_for_non_ref_pic,
                    *offset_for_top_to_bottom_field,
                    frame_num_offset,
                    d0,
                    d1,
                )?
            }
            PicOrderCntType::TypeTwo => {
                let frame_num_offset = if is_reference {
                    self.derive_frame_num_offset(
                        sps,
                        current_pic_num,
                        is_field,
                        origin_reset,
                        None,
                    )?
                } else {
                    self.derive_non_ref_frame_num_offset(
                        sps,
                        current_pic_num,
                        is_field,
                        None,
                    )?
                };
                // Type 2 has no delta syntax and no non-ref/top-to-bottom offsets.
                self.record_type_one(
                    header,
                    pps,
                    current_pic_num,
                    is_field,
                    is_bottom_field,
                    origin_reset,
                    is_reference,
                    0,
                    0,
                    frame_num_offset,
                    0,
                    0,
                )?
            }
        };

        if is_reference {
            self.prev_ref_pic_num = current_pic_num;
            self.have_prev_ref = true;
        }
        Ok(result)
    }

    /// `FrameNumOffset` derivation for a reference picture (spec 8.2.1.2.1), committing
    /// the updated offset and cycle-pointer state. `offsets_for_ref_frame` is `None`
    /// for type 2, where every reference frame position contributes a fixed offset of
    /// two.
    fn derive_frame_num_offset(
        &mut self,
        sps: &SeqParameterSet,
        current_pic_num: u32,
        is_field: bool,
        origin_reset: bool,
        offsets_for_ref_frame: Option<&[i32]>,
    ) -> Result<i64, PicOrderCntError> {
        let max_frame_num: u32 = 1u32
            .checked_shl(u32::from(sps.log2_max_frame_num()))
            .ok_or(PicOrderCntError::IntegerOverflow)?;

        if origin_reset {
            self.prev_frame_num_offset = 0;
            self.cycle = CycleState::default();
            return Ok(0);
        }

        if !self.have_prev_ref {
            return Err(PicOrderCntError::NoReferencePoint);
        }

        let (offset, cycle) = Self::compute_frame_num_offset(
            self.prev_frame_num_offset,
            self.prev_ref_pic_num,
            self.cycle.clone(),
            max_frame_num,
            current_pic_num,
            is_field,
            offsets_for_ref_frame,
        )?;
        self.prev_frame_num_offset = offset;
        self.cycle = cycle;
        Ok(offset)
    }

    /// `FrameNumOffset` derivation for a non-reference picture. The spec requires the
    /// same gap accounting but leaves the stored state untouched (the values track the
    /// previous *reference* picture only), so the cycle pointer is advanced on a local
    /// copy and discarded.
    fn derive_non_ref_frame_num_offset(
        &self,
        sps: &SeqParameterSet,
        current_pic_num: u32,
        is_field: bool,
        offsets_for_ref_frame: Option<&[i32]>,
    ) -> Result<i64, PicOrderCntError> {
        let max_frame_num: u32 = 1u32
            .checked_shl(u32::from(sps.log2_max_frame_num()))
            .ok_or(PicOrderCntError::IntegerOverflow)?;
        if !self.have_prev_ref {
            return Err(PicOrderCntError::NoReferencePoint);
        }
        let (offset, _cycle) = Self::compute_frame_num_offset(
            self.prev_frame_num_offset,
            self.prev_ref_pic_num,
            self.cycle.clone(),
            max_frame_num,
            current_pic_num,
            is_field,
            offsets_for_ref_frame,
        )?;
        Ok(offset)
    }

    /// Gap accounting shared by reference and non-reference pictures. Returns the new
    /// offset alongside the (possibly advanced) cycle state.
    fn compute_frame_num_offset(
        prev_frame_num_offset: i64,
        prev_ref_pic_num: u32,
        mut cycle: CycleState,
        max_frame_num: u32,
        current_pic_num: u32,
        is_field: bool,
        offsets_for_ref_frame: Option<&[i32]>,
    ) -> Result<(i64, CycleState), PicOrderCntError> {
        // Frame positions of the previous and current pictures. Field pictures use the
        // doubled identification (2*frame_num + bottom_field_flag), so dividing by two
        // pairs up the two fields of a frame.
        let prev_frame_pos = if is_field {
            prev_ref_pic_num / 2
        } else {
            prev_ref_pic_num
        };
        let current_frame_pos = if is_field {
            current_pic_num / 2
        } else {
            current_pic_num
        };

        // Offset accumulated over the frame positions between the two pictures. On a
        // frame_num wrap the gap spans two ranges (..MaxFrameNum and ..current),
        // matching the two-loop form in the spec.
        let mut extra = 0i64;
        if current_frame_pos < prev_frame_pos {
            for _ in prev_frame_pos..max_frame_num {
                extra = extra
                    .checked_add(Self::next_cycle_increment(
                        &mut cycle,
                        offsets_for_ref_frame,
                    )?)
                    .ok_or(PicOrderCntError::IntegerOverflow)?;
            }
        }
        let start = if current_frame_pos < prev_frame_pos {
            0
        } else {
            prev_frame_pos
        };
        for _ in start..current_frame_pos {
            extra = extra
                .checked_add(Self::next_cycle_increment(
                    &mut cycle,
                    offsets_for_ref_frame,
                )?)
                .ok_or(PicOrderCntError::IntegerOverflow)?;
        }
        let offset = prev_frame_num_offset
            .checked_add(extra)
            .ok_or(PicOrderCntError::IntegerOverflow)?;
        Ok((offset, cycle))
    }

    /// Increment contributed by one reference frame position, advancing the type-1
    /// cycle pointer. Type 2 always contributes two.
    fn next_cycle_increment(
        cycle: &mut CycleState,
        offsets_for_ref_frame: Option<&[i32]>,
    ) -> Result<i64, PicOrderCntError> {
        match offsets_for_ref_frame {
            Some(offsets) => {
                let cycle_len = offsets.len();
                if cycle_len == 0 {
                    return Err(PicOrderCntError::InvalidPicOrderCntCycle);
                }
                // The cycle starts at offsets[0]; after consuming the final entry the
                // index wraps so the next frame position again begins at offsets[0].
                let idx = cycle.index;
                let v = i64::from(offsets[idx]);
                cycle.index = if idx + 1 == cycle_len { 0 } else { idx + 1 };
                Ok(v)
            }
            None => Ok(2),
        }
    }

    /// Type 0 derivation (spec 8.2.1.1).
    #[allow(clippy::too_many_arguments)]
    fn record_type_zero(
        &mut self,
        header: &SliceHeader,
        current_pic_num: u32,
        max_pic_order_cnt_lsb: i64,
        is_field: bool,
        is_bottom_field: bool,
        origin_reset: bool,
        is_reference: bool,
    ) -> Result<PicOrder, PicOrderCntError> {
        let (lsb, delta_bottom) = match header.pic_order_cnt_lsb {
            Some(PicOrderCountLsb::Frame(v)) => (i64::from(v), None),
            Some(PicOrderCountLsb::FieldsAbsolute {
                pic_order_cnt_lsb,
                delta_pic_order_cnt_bottom,
            }) => (
                i64::from(pic_order_cnt_lsb),
                Some(i64::from(delta_pic_order_cnt_bottom)),
            ),
            // Type 0 never produces FieldsDelta; treat anything else as a logic error.
            Some(PicOrderCountLsb::FieldsDelta(_)) | None => {
                return Err(PicOrderCntError::IncompatibleSequenceParameters(
                    "slice header carries no pic_order_cnt_lsb despite pic_order_cnt_type == 0",
                ));
            }
        };

        let pic_order_cnt_msb = if origin_reset {
            0
        } else if lsb < self.prev_pic_order_cnt_lsb
            && self.prev_pic_order_cnt_lsb - lsb >= max_pic_order_cnt_lsb / 2
        {
            self.prev_pic_order_cnt_msb
                .checked_add(max_pic_order_cnt_lsb)
                .ok_or(PicOrderCntError::IntegerOverflow)?
        } else if lsb > self.prev_pic_order_cnt_lsb
            && lsb - self.prev_pic_order_cnt_lsb > max_pic_order_cnt_lsb / 2
        {
            self.prev_pic_order_cnt_msb
                .checked_sub(max_pic_order_cnt_lsb)
                .ok_or(PicOrderCntError::IntegerOverflow)?
        } else {
            self.prev_pic_order_cnt_msb
        };

        let top_field_pic_order_cnt = pic_order_cnt_msb
            .checked_add(lsb)
            .ok_or(PicOrderCntError::IntegerOverflow)?;

        let result = if is_field {
            if is_bottom_field {
                if origin_reset {
                    // IDR bottom field: both field counts start at zero.
                    PicOrder::Field {
                        field: FieldOrder::Bottom,
                        pic_order_cnt: lsb,
                    }
                } else {
                    // The preceding reference picture must be the top field of the same
                    // frame (same doubled frame-number pair).
                    if !self.have_prev_top || self.prev_top_pic_num != current_pic_num - 1 {
                        return Err(PicOrderCntError::MissingPreviousTopField);
                    }
                    let bottom = self
                        .prev_top_field_pic_order_cnt
                        .checked_add(top_field_pic_order_cnt)
                        .ok_or(PicOrderCntError::IntegerOverflow)?;
                    PicOrder::Field {
                        field: FieldOrder::Bottom,
                        pic_order_cnt: bottom,
                    }
                }
            } else {
                PicOrder::Field {
                    field: FieldOrder::Top,
                    pic_order_cnt: top_field_pic_order_cnt,
                }
            }
        } else {
            let bottom_field_pic_order_cnt = if let Some(delta) = delta_bottom {
                top_field_pic_order_cnt
                    .checked_add(delta)
                    .ok_or(PicOrderCntError::IntegerOverflow)?
            } else if self.key.as_ref().is_some_and(|k| k.fields_supported) {
                // No explicit bottom delta: fields interleave at one count per field.
                top_field_pic_order_cnt
                    .checked_add(1)
                    .ok_or(PicOrderCntError::IntegerOverflow)?
            } else {
                // frame_mbs_only_flag == 1: the two fields share one count.
                top_field_pic_order_cnt
            };
            PicOrder::Frame {
                top_field_pic_order_cnt,
                bottom_field_pic_order_cnt,
            }
        };

        // State update (8.2.1.1 end). Bottom fields do not update the MSB/LSB variables.
        if is_reference && !(is_field && is_bottom_field) {
            self.prev_pic_order_cnt_msb = pic_order_cnt_msb;
            self.prev_pic_order_cnt_lsb = lsb;
        }
        if is_reference {
            match &result {
                PicOrder::Frame {
                    top_field_pic_order_cnt,
                    ..
                } => {
                    self.prev_top_field_pic_order_cnt = *top_field_pic_order_cnt;
                    self.prev_top_pic_num = current_pic_num;
                    self.have_prev_top = true;
                }
                PicOrder::Field {
                    field: FieldOrder::Top,
                    pic_order_cnt,
                } => {
                    self.prev_top_field_pic_order_cnt = *pic_order_cnt;
                    self.prev_top_pic_num = current_pic_num;
                    self.have_prev_top = true;
                }
                // The spec keeps PrevBottomFieldPicOrderCnt separately, but for bottom
                // fields the derivation only ever consults the preceding top field.
                PicOrder::Field {
                    field: FieldOrder::Bottom,
                    ..
                } => {
                    self.have_prev_top = false;
                }
            }
        }
        Ok(result)
    }

    /// Shared derivation for types 1 and 2 (spec 8.2.1.2.2); type 2 passes zero for all
    /// offsets and deltas.
    #[allow(clippy::too_many_arguments)]
    fn record_type_one(
        &mut self,
        header: &SliceHeader,
        pps: &PicParameterSet,
        current_pic_num: u32,
        is_field: bool,
        is_bottom_field: bool,
        origin_reset: bool,
        is_reference: bool,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        frame_num_offset: i64,
        delta_pic_order_cnt0: i32,
        delta_pic_order_cnt1: i32,
    ) -> Result<PicOrder, PicOrderCntError> {
        let delta0 = i64::from(delta_pic_order_cnt0);
        let delta1 = i64::from(delta_pic_order_cnt1);

        let result = if origin_reset {
            // IDR / MMCO 5: both field counts start at zero, then deltas apply.
            if is_field && is_bottom_field {
                PicOrder::Field {
                    field: FieldOrder::Bottom,
                    pic_order_cnt: delta0,
                }
            } else if is_field {
                PicOrder::Field {
                    field: FieldOrder::Top,
                    pic_order_cnt: delta0,
                }
            } else {
                let bottom = self.frame_bottom_count(header, pps, delta0, delta1)?;
                PicOrder::Frame {
                    top_field_pic_order_cnt: delta0,
                    bottom_field_pic_order_cnt: bottom,
                }
            }
        } else if is_field && is_bottom_field {
            // The preceding reference picture must be the top field of the same frame.
            if !self.have_prev_top || self.prev_top_pic_num != current_pic_num - 1 {
                return Err(PicOrderCntError::MissingPreviousTopField);
            }
            let expected = self
                .prev_top_field_pic_order_cnt
                .checked_add(
                    frame_num_offset
                        .checked_mul(2)
                        .ok_or(PicOrderCntError::IntegerOverflow)?,
                )
                .ok_or(PicOrderCntError::IntegerOverflow)?
                .checked_add(
                    i64::from(offset_for_top_to_bottom_field)
                        .checked_mul(2)
                        .ok_or(PicOrderCntError::IntegerOverflow)?,
                )
                .ok_or(PicOrderCntError::IntegerOverflow)?;
            PicOrder::Field {
                field: FieldOrder::Bottom,
                pic_order_cnt: expected
                    .checked_add(delta0)
                    .ok_or(PicOrderCntError::IntegerOverflow)?,
            }
        } else {
            let mut expected_pic_order_cnt = frame_num_offset;
            if !is_reference {
                expected_pic_order_cnt = expected_pic_order_cnt
                    .checked_add(i64::from(offset_for_non_ref_pic))
                    .ok_or(PicOrderCntError::IntegerOverflow)?;
            }
            let top = expected_pic_order_cnt
                .checked_add(delta0)
                .ok_or(PicOrderCntError::IntegerOverflow)?;
            if is_field {
                PicOrder::Field {
                    field: FieldOrder::Top,
                    pic_order_cnt: top,
                }
            } else {
                let bottom = self.frame_bottom_count(header, pps, top, delta1)?;
                PicOrder::Frame {
                    top_field_pic_order_cnt: top,
                    bottom_field_pic_order_cnt: bottom,
                }
            }
        };

        // State update (8.2.1.3). The frame_num offset and cycle pointer were already
        // committed in derive_frame_num_offset when this picture is a reference.
        if is_reference {
            match &result {
                PicOrder::Frame {
                    top_field_pic_order_cnt,
                    ..
                } => {
                    self.prev_top_field_pic_order_cnt = *top_field_pic_order_cnt;
                    self.prev_top_pic_num = current_pic_num;
                    self.have_prev_top = true;
                }
                PicOrder::Field {
                    field: FieldOrder::Top,
                    pic_order_cnt,
                } => {
                    self.prev_top_field_pic_order_cnt = *pic_order_cnt;
                    self.prev_top_pic_num = current_pic_num;
                    self.have_prev_top = true;
                }
                // Only the preceding top-field count feeds the bottom-field formula.
                PicOrder::Field {
                    field: FieldOrder::Bottom,
                    ..
                } => {
                    self.have_prev_top = false;
                }
            }
        }
        Ok(result)
    }

    /// Bottom-field count of a frame picture: the explicit delta from the slice header
    /// when present, otherwise the conventional one-count-per-field progression.
    fn frame_bottom_count(
        &self,
        header: &SliceHeader,
        pps: &PicParameterSet,
        top: i64,
        delta1: i64,
    ) -> Result<i64, PicOrderCntError> {
        if let Some(PicOrderCountLsb::FieldsDelta([_, _])) = header.pic_order_cnt_lsb {
            return top
                .checked_add(delta1)
                .ok_or(PicOrderCntError::IntegerOverflow);
        }
        if let Some(PicOrderCountLsb::FieldsAbsolute {
            delta_pic_order_cnt_bottom,
            ..
        }) = header.pic_order_cnt_lsb
        {
            return top
                .checked_add(i64::from(delta_pic_order_cnt_bottom))
                .ok_or(PicOrderCntError::IntegerOverflow);
        }
        // No explicit bottom delta. When the sequence supports fields (either promised
        // by the PPS flag or by frame_mbs_only_flag == 0), fields interleave at one
        // count per field; otherwise the two fields of a frame share one count.
        let fields_supported =
            pps.bottom_field_pic_order_in_frame_present_flag
                || self.key.as_ref().is_some_and(|k| k.fields_supported);
        if fields_supported {
            top.checked_add(1)
                .ok_or(PicOrderCntError::IntegerOverflow)
        } else {
            Ok(top)
        }
    }
}

#[cfg(test)]
mod test;
