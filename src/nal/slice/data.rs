//! Slice data access (currently baseline profile - CAVLC only, no MBAFF).
//!
//! Implements the `slice_data()` syntax from spec section 7.3.4.
//!
//! 1. Parse context-free parts of `macroblock_layer()` — `mb_type`, `mb_pred`/`sub_mb_pred`,
//!    `coded_block_pattern`, `mb_qp_delta`.
//! 2. The caller takes over the `BitRead` stream to parse `residual()` using CAVLC with
//!    decoder-computed `nC` values.

pub use crate::nal::slice::cavlc::{CavlcContext, CavlcError};
use crate::nal::slice::fmo::{SliceGroupMap, SliceGroupMapError};
use crate::nal::slice::macroblock::{
    coded_block_pattern_from_me, i_mb_type_info, p_mb_type_info, p_sub_mb_type_info, IMbTypeInfo,
    MbPartPredMode, MbTypeError, PMbTypeInfo, SubMbTypeInfo,
};
use crate::nal::slice::{cavlc, SliceFamily};
use crate::rbsp::{BitRead, BitReaderError};
use std::convert::TryFrom;
use std::fmt;

#[derive(Debug)]
pub enum SliceDataError {
    BitReaderError(BitReaderError),
    InvalidMbType(MbTypeError),
    InvalidSubMbType(MbTypeError),
    InvalidCodedBlockPattern(u32),
    InvalidParameter {
        field: &'static str,
        value: i64,
    },
    /// The PPS slice-group declaration could not be turned into a map for this picture.
    InvalidSliceGroup(SliceGroupMapError),
    /// `first_mb_in_slice` names a map unit not present in the picture, or one that
    /// cannot be located in its slice group.
    InvalidFirstMbInSlice(u32),
}

impl fmt::Display for SliceDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SliceDataError::BitReaderError(e) => write!(f, "bitstream I/O: {e:?}"),
            SliceDataError::InvalidMbType(e) => write!(f, "{e}"),
            SliceDataError::InvalidSubMbType(e) => write!(f, "{e}"),
            SliceDataError::InvalidCodedBlockPattern(v) => {
                write!(f, "invalid coded_block_pattern code_num {v}")
            }
            SliceDataError::InvalidParameter { field, value } => {
                write!(f, "{field} out of range: {value}")
            }
            SliceDataError::InvalidSliceGroup(e) => write!(f, "invalid slice group map: {e}"),
            SliceDataError::InvalidFirstMbInSlice(v) => {
                write!(f, "first_mb_in_slice {v} is not a valid map unit address")
            }
        }
    }
}

impl std::error::Error for SliceDataError {}

impl From<BitReaderError> for SliceDataError {
    fn from(e: BitReaderError) -> Self {
        SliceDataError::BitReaderError(e)
    }
}

impl From<SliceGroupMapError> for SliceDataError {
    fn from(e: SliceGroupMapError) -> Self {
        SliceDataError::InvalidSliceGroup(e)
    }
}

impl From<MbTypeError> for SliceDataError {
    fn from(e: MbTypeError) -> Self {
        match e {
            MbTypeError::InvalidPSubMbType(_) => SliceDataError::InvalidSubMbType(e),
            _ => SliceDataError::InvalidMbType(e),
        }
    }
}

#[derive(Debug)]
pub struct InterPrediction {
    pub num_parts: u8,
    pub part_width: u8,
    pub part_height: u8,
    pub ref_idx_l0: [i8; 4],
    pub mvd_l0: [[[i16; 2]; 4]; 4],
    pub sub_mb_info: Option<SubMbInfo>,
}

#[derive(Debug)]
pub struct SubMbInfo {
    pub sub_mb_type: [u8; 4],
    pub info: [SubMbTypeInfo; 4],
    pub ref_idx_l0: [i8; 4],
}

#[derive(Debug)]
pub enum MbPrediction {
    Intra4x4 {
        prev_intra4x4_pred_mode_flag: [bool; 16],
        rem_intra4x4_pred_mode: [u8; 16],
        intra_chroma_pred_mode: u8,
    },
    Intra16x16 {
        intra_chroma_pred_mode: u8,
    },
    IntraPCM {
        pcm_sample_luma: [u8; 256],
        pcm_sample_chroma: [u8; 128],
    },
    Inter(InterPrediction),
}

#[derive(Debug)]
pub enum MbTypeInfo {
    I(IMbTypeInfo),
    P(PMbTypeInfo),
}

#[derive(Debug)]
pub struct MacroblockHeader {
    pub mb_addr: u32,
    pub mb_type: MbTypeInfo,
    pub coded_block_pattern_luma: u8,
    pub coded_block_pattern_chroma: u8,
    pub mb_qp_delta: i32,
    pub mb_pred: MbPrediction,
}

pub enum SliceEvent<'a, R: BitRead> {
    End,
    Skip {
        mb_addr: u32,
        next: SliceDataReader<'a, R>,
    },
    Macroblock(MacroblockResidual<'a, R>),
}

struct SliceDataCore<'a, R: BitRead> {
    reader: &'a mut R,
    slice_family: SliceFamily,
    /// Slice group this slice belongs to.
    group: u8,
    /// Position within the slice group's ascending map-unit address list.
    group_index: usize,
    /// Raster-scan map unit address of the macroblock currently being read.
    curr_mb_addr: u32,
    /// Number of map units in the slice group (the slice covers all of them).
    group_len: usize,
    /// The full per-picture slice group map, retained so callers can inspect group
    /// assignments while iterating.
    group_map: SliceGroupMap,
    num_ref_idx_l0_active_minus1: u32,
    pending_skips: u32,
    after_skip_run: bool,
    done: bool,
}

impl<R: BitRead> SliceDataCore<'_, R> {
    /// Advance to the next map unit of this slice group, returning its raster-scan
    /// address, or `None` when every map unit of the group has been visited.
    fn advance_mb_address(&mut self) -> Option<u32> {
        self.group_index += 1;
        if self.group_index >= self.group_len {
            self.done = true;
            None
        } else {
            let addr = self
                .group_map
                .addr_in_group(self.group, self.group_index)
                .expect("group_index is bounded by the group's address list length");
            self.curr_mb_addr = addr;
            Some(addr)
        }
    }

    fn parse_macroblock_header(
        &mut self,
        mb_addr: u32,
    ) -> Result<MacroblockHeader, SliceDataError> {
        let raw_mb_type = self.reader.read_ue("mb_type")?;
        log::trace!(
            "MB {}: raw_mb_type={}, slice_family={:?}",
            mb_addr,
            raw_mb_type,
            self.slice_family
        );

        match self.slice_family {
            SliceFamily::I | SliceFamily::SI => {
                let info = i_mb_type_info(raw_mb_type)?;
                self.parse_i_macroblock(mb_addr, info)
            }
            SliceFamily::P | SliceFamily::SP => {
                let info = p_mb_type_info(raw_mb_type)?;
                self.parse_p_macroblock(mb_addr, info)
            }
            SliceFamily::B => {
                // B slices not supported yet (baseline profile only)
                Err(SliceDataError::InvalidParameter {
                    field: "slice_type",
                    value: 1,
                })
            }
        }
    }

    fn parse_i_macroblock(
        &mut self,
        mb_addr: u32,
        info: IMbTypeInfo,
    ) -> Result<MacroblockHeader, SliceDataError> {
        let (mb_pred, cbp_luma, cbp_chroma) = self.parse_i_mb_body(&info)?;

        let mb_qp_delta =
            if cbp_luma > 0 || cbp_chroma > 0 || matches!(info, IMbTypeInfo::I16x16 { .. }) {
                self.read_mb_qp_delta()?
            } else {
                0
            };

        Ok(MacroblockHeader {
            mb_addr,
            mb_type: MbTypeInfo::I(info),
            coded_block_pattern_luma: cbp_luma,
            coded_block_pattern_chroma: cbp_chroma,
            mb_qp_delta,
            mb_pred,
        })
    }

    fn parse_p_macroblock(
        &mut self,
        mb_addr: u32,
        info: PMbTypeInfo,
    ) -> Result<MacroblockHeader, SliceDataError> {
        match info {
            PMbTypeInfo::I(ref i_info) => {
                let (mb_pred, cbp_luma, cbp_chroma) = self.parse_i_mb_body(i_info)?;

                let mb_qp_delta = if cbp_luma > 0
                    || cbp_chroma > 0
                    || matches!(i_info, IMbTypeInfo::I16x16 { .. })
                {
                    self.read_mb_qp_delta()?
                } else {
                    0
                };

                Ok(MacroblockHeader {
                    mb_addr,
                    mb_type: MbTypeInfo::P(info),
                    coded_block_pattern_luma: cbp_luma,
                    coded_block_pattern_chroma: cbp_chroma,
                    mb_qp_delta,
                    mb_pred,
                })
            }
            PMbTypeInfo::P {
                num_parts,
                part_width,
                part_height,
                ref_idx_forced_zero,
                ..
            } => {
                let is_8x8 = num_parts == 4;

                let mb_pred = if is_8x8 {
                    self.parse_sub_mb_pred(ref_idx_forced_zero)?
                } else {
                    self.parse_inter_mb_pred(num_parts, part_width, part_height)?
                };

                // coded_block_pattern is read as ue(v), then mapped.
                let code_num = self.reader.read_ue("coded_block_pattern")?;
                let (cbp_luma, cbp_chroma) = coded_block_pattern_from_me(code_num, false)
                    .ok_or(SliceDataError::InvalidCodedBlockPattern(code_num))?;

                let mb_qp_delta = if cbp_luma > 0 || cbp_chroma > 0 {
                    self.read_mb_qp_delta()?
                } else {
                    0
                };

                Ok(MacroblockHeader {
                    mb_addr,
                    mb_type: MbTypeInfo::P(info),
                    coded_block_pattern_luma: cbp_luma,
                    coded_block_pattern_chroma: cbp_chroma,
                    mb_qp_delta,
                    mb_pred,
                })
            }
        }
    }

    fn parse_i_mb_body(
        &mut self,
        info: &IMbTypeInfo,
    ) -> Result<(MbPrediction, u8, u8), SliceDataError> {
        match info {
            IMbTypeInfo::INxN => {
                let mut prev_flags = [false; 16];
                let mut rem_modes = [0u8; 16];
                for i in 0..16 {
                    prev_flags[i] = self.reader.read_bit("prev_intra4x4_pred_mode_flag")?;
                    if !prev_flags[i] {
                        rem_modes[i] = self.reader.read::<3, u8>("rem_intra4x4_pred_mode")?;
                    }
                }
                let intra_chroma_pred_mode_val = self.reader.read_ue("intra_chroma_pred_mode")?;
                if intra_chroma_pred_mode_val > 3 {
                    return Err(SliceDataError::InvalidParameter {
                        field: "intra_chroma_pred_mode",
                        value: intra_chroma_pred_mode_val as i64,
                    });
                }
                let intra_chroma_pred_mode = intra_chroma_pred_mode_val as u8;

                let code_num = self.reader.read_ue("coded_block_pattern")?;
                let (cbp_luma, cbp_chroma) = coded_block_pattern_from_me(code_num, true)
                    .ok_or(SliceDataError::InvalidCodedBlockPattern(code_num))?;

                Ok((
                    MbPrediction::Intra4x4 {
                        prev_intra4x4_pred_mode_flag: prev_flags,
                        rem_intra4x4_pred_mode: rem_modes,
                        intra_chroma_pred_mode,
                    },
                    cbp_luma,
                    cbp_chroma,
                ))
            }
            IMbTypeInfo::I16x16 {
                coded_block_pattern_luma,
                coded_block_pattern_chroma,
                ..
            } => {
                let intra_chroma_pred_mode_val = self.reader.read_ue("intra_chroma_pred_mode")?;
                if intra_chroma_pred_mode_val > 3 {
                    return Err(SliceDataError::InvalidParameter {
                        field: "intra_chroma_pred_mode",
                        value: intra_chroma_pred_mode_val as i64,
                    });
                }
                let intra_chroma_pred_mode = intra_chroma_pred_mode_val as u8;

                Ok((
                    MbPrediction::Intra16x16 {
                        intra_chroma_pred_mode,
                    },
                    *coded_block_pattern_luma,
                    *coded_block_pattern_chroma,
                ))
            }
            IMbTypeInfo::IPCM => {
                while !self.reader.byte_aligned() {
                    if self.reader.read_bit("pcm_alignment_zero_bit")? {
                        return Err(SliceDataError::InvalidParameter {
                            field: "pcm_alignment_zero_bit",
                            value: 1,
                        });
                    }
                }

                let mut pcm_sample_luma = [0u8; 256];
                for sample in &mut pcm_sample_luma {
                    *sample = self.reader.read::<8, u8>("pcm_sample_luma")?;
                }
                let mut pcm_sample_chroma = [0u8; 128];
                for sample in &mut pcm_sample_chroma {
                    *sample = self.reader.read::<8, u8>("pcm_sample_chroma")?;
                }

                Ok((
                    MbPrediction::IntraPCM {
                        pcm_sample_luma,
                        pcm_sample_chroma,
                    },
                    0,
                    0,
                ))
            }
        }
    }

    fn parse_inter_mb_pred(
        &mut self,
        num_parts: u8,
        part_width: u8,
        part_height: u8,
    ) -> Result<MbPrediction, SliceDataError> {
        let mut ref_idx_l0 = [0i8; 4];
        let mut mvd_l0 = [[[0i16; 2]; 4]; 4];

        for item in ref_idx_l0.iter_mut().take(num_parts as usize) {
            *item = self.read_te("ref_idx_l0", self.num_ref_idx_l0_active_minus1)?;
        }

        for item in mvd_l0.iter_mut().take(num_parts as usize) {
            let x = self.reader.read_se("mvd_l0_x")?;
            item[0][0] = i16::try_from(x).map_err(|_| SliceDataError::InvalidParameter {
                field: "mvd_l0_x",
                value: x as i64,
            })?;
            let y = self.reader.read_se("mvd_l0_y")?;
            item[0][1] = i16::try_from(y).map_err(|_| SliceDataError::InvalidParameter {
                field: "mvd_l0_y",
                value: y as i64,
            })?;
        }

        Ok(MbPrediction::Inter(InterPrediction {
            num_parts,
            part_width,
            part_height,
            ref_idx_l0,
            mvd_l0,
            sub_mb_info: None,
        }))
    }

    fn parse_sub_mb_pred(
        &mut self,
        ref_idx_forced_zero: bool,
    ) -> Result<MbPrediction, SliceDataError> {
        let mut sub_mb_type_raw = [0u8; 4];
        let mut sub_info: [SubMbTypeInfo; 4] = [
            SubMbTypeInfo {
                num_sub_parts: 1,
                sub_part_width: 8,
                sub_part_height: 8,
                pred_mode: MbPartPredMode::PredL0,
            },
            SubMbTypeInfo {
                num_sub_parts: 1,
                sub_part_width: 8,
                sub_part_height: 8,
                pred_mode: MbPartPredMode::PredL0,
            },
            SubMbTypeInfo {
                num_sub_parts: 1,
                sub_part_width: 8,
                sub_part_height: 8,
                pred_mode: MbPartPredMode::PredL0,
            },
            SubMbTypeInfo {
                num_sub_parts: 1,
                sub_part_width: 8,
                sub_part_height: 8,
                pred_mode: MbPartPredMode::PredL0,
            },
        ];
        for i in 0..4 {
            let raw = self.reader.read_ue("sub_mb_type")?;
            sub_mb_type_raw[i] = raw as u8;
            sub_info[i] = p_sub_mb_type_info(raw)?;
        }

        let mut sub_ref_idx_l0 = [0i8; 4];
        if !ref_idx_forced_zero {
            for item in &mut sub_ref_idx_l0 {
                *item = self.read_te("ref_idx_l0", self.num_ref_idx_l0_active_minus1)?;
            }
        }

        let mut mvd_l0 = [[[0i16; 2]; 4]; 4];
        for mb_part in 0..4 {
            for sub_part in 0..sub_info[mb_part].num_sub_parts as usize {
                let x = self.reader.read_se("mvd_l0_x")?;
                mvd_l0[mb_part][sub_part][0] =
                    i16::try_from(x).map_err(|_| SliceDataError::InvalidParameter {
                        field: "mvd_l0_x",
                        value: x as i64,
                    })?;
                let y = self.reader.read_se("mvd_l0_y")?;
                mvd_l0[mb_part][sub_part][1] =
                    i16::try_from(y).map_err(|_| SliceDataError::InvalidParameter {
                        field: "mvd_l0_y",
                        value: y as i64,
                    })?;
            }
        }

        Ok(MbPrediction::Inter(InterPrediction {
            num_parts: 4,
            part_width: 8,
            part_height: 8,
            ref_idx_l0: [0; 4],
            mvd_l0,
            sub_mb_info: Some(SubMbInfo {
                sub_mb_type: sub_mb_type_raw,
                info: sub_info,
                ref_idx_l0: sub_ref_idx_l0,
            }),
        }))
    }

    fn read_mb_qp_delta(&mut self) -> Result<i32, SliceDataError> {
        let val = self.reader.read_se("mb_qp_delta")?;
        if !(-26..=25).contains(&val) {
            return Err(SliceDataError::InvalidParameter {
                field: "mb_qp_delta",
                value: val as i64,
            });
        }
        Ok(val)
    }

    /// Read a `te(v)` (truncated Exp-Golomb) value.
    ///
    /// - If `max == 0`: the value is always 0 (nothing is read).
    /// - If `max == 1`: read 1 bit and invert (0 -> 1, 1 -> 0).
    /// - If `max > 1`: read as `ue(v)`.
    fn read_te(&mut self, name: &'static str, max: u32) -> Result<i8, SliceDataError> {
        if max == 0 {
            Ok(0)
        } else if max == 1 {
            let bit = self.reader.read_bit(name)?;
            Ok(if bit { 0 } else { 1 })
        } else {
            let val = self.reader.read_ue(name)?;
            if val > max {
                return Err(SliceDataError::InvalidParameter {
                    field: name,
                    value: val as i64,
                });
            }
            Ok(val as i8)
        }
    }
}

pub struct SliceDataReader<'a, R: BitRead> {
    core: SliceDataCore<'a, R>,
}

impl<'a, R: BitRead> SliceDataReader<'a, R> {
    /// Create a reader for a slice whose macroblocks are visited in plain raster order
    /// (a single-slice-group PPS).
    ///
    /// Returns [`SliceDataError::InvalidFirstMbInSlice`] when `first_mb_in_slice` is not
    /// a valid map unit address within `pic_size_in_mbs`. For PPSes that declare multiple
    /// slice groups, build a [`crate::nal::slice::fmo::SliceGroupMap`] and use
    /// [`Self::with_group_map()`] instead.
    pub fn new(
        reader: &'a mut R,
        slice_family: SliceFamily,
        first_mb_in_slice: u32,
        pic_size_in_mbs: u32,
        num_ref_idx_l0_active_minus1: u32,
    ) -> Result<Self, SliceDataError> {
        let group_map = SliceGroupMap::single_group(pic_size_in_mbs);
        Self::with_group_map(
            reader,
            slice_family,
            first_mb_in_slice,
            group_map,
            num_ref_idx_l0_active_minus1,
        )
    }

    /// Create a reader that walks macroblocks according to `group_map`, as required when
    /// the active PPS declares more than one slice group.
    ///
    /// The map must describe the picture that the slice belongs to (rebuilt per picture
    /// for map types 3–5 using `slice_group_change_cycle`). The reader locates
    /// `first_mb_in_slice` inside its slice group and then visits that group's map units
    /// in ascending raster-scan order, so the macroblocks of every slice in the picture
    /// together cover each map unit exactly once.
    pub fn with_group_map(
        reader: &'a mut R,
        slice_family: SliceFamily,
        first_mb_in_slice: u32,
        group_map: SliceGroupMap,
        num_ref_idx_l0_active_minus1: u32,
    ) -> Result<Self, SliceDataError> {
        let (group, group_index) = group_map
            .group_position(first_mb_in_slice)
            .ok_or(SliceDataError::InvalidFirstMbInSlice(first_mb_in_slice))?;
        let group_len = group_map.group_len(group);
        Ok(SliceDataReader {
            core: SliceDataCore {
                reader,
                slice_family,
                group,
                group_index,
                curr_mb_addr: first_mb_in_slice,
                group_len,
                group_map,
                num_ref_idx_l0_active_minus1,
                pending_skips: 0,
                after_skip_run: false,
                done: false,
            },
        })
    }

    /// The slice group map in effect for this slice.
    pub fn group_map(&self) -> &SliceGroupMap {
        &self.core.group_map
    }

    pub fn next(mut self) -> Result<SliceEvent<'a, R>, SliceDataError> {
        if self.core.done {
            return Ok(SliceEvent::End);
        }

        if self.core.pending_skips > 0 {
            self.core.pending_skips -= 1;
            let addr = self.core.curr_mb_addr;
            // A skip run that overruns the slice group is a malformed stream; stop at the
            // group boundary rather than emitting an out-of-range address.
            let group_exhausted = self.core.advance_mb_address().is_none();
            if group_exhausted {
                self.core.done = true;
            } else if self.core.pending_skips == 0 {
                if !self.core.reader.has_more_rbsp_data("slice_data")? {
                    self.core.done = true;
                } else {
                    self.core.after_skip_run = true;
                }
            }
            return Ok(SliceEvent::Skip {
                mb_addr: addr,
                next: self,
            });
        }

        if self.core.after_skip_run {
            self.core.after_skip_run = false;
        } else if self.core.slice_family != SliceFamily::I
            && self.core.slice_family != SliceFamily::SI
        {
            let mb_skip_run = self.core.reader.read_ue("mb_skip_run")?;
            if mb_skip_run > 0 {
                self.core.pending_skips = mb_skip_run;
                self.core.pending_skips -= 1;
                let addr = self.core.curr_mb_addr;
                let group_exhausted = self.core.advance_mb_address().is_none();
                if self.core.pending_skips == 0 {
                    if group_exhausted || !self.core.reader.has_more_rbsp_data("slice_data")? {
                        self.core.done = true;
                    } else {
                        self.core.after_skip_run = true;
                    }
                }
                return Ok(SliceEvent::Skip {
                    mb_addr: addr,
                    next: self,
                });
            }
            if !self.core.reader.has_more_rbsp_data("slice_data")? {
                self.core.done = true;
                return Ok(SliceEvent::End);
            }
        }

        let mb_addr = self.core.curr_mb_addr;
        let header = self.core.parse_macroblock_header(mb_addr)?;
        // The group is exhausted once there is no further map unit to visit; the caller
        // still gets the parsed macroblock via `MacroblockResidual`.
        self.core.advance_mb_address();

        Ok(SliceEvent::Macroblock(MacroblockResidual {
            core: self.core,
            header,
        }))
    }

    pub fn curr_mb_addr(&self) -> u32 {
        self.core.curr_mb_addr
    }

    pub fn pic_size_in_mbs(&self) -> u32 {
        self.core.group_map.pic_size_in_map_units()
    }
}

pub struct MacroblockResidual<'a, R: BitRead> {
    core: SliceDataCore<'a, R>,
    header: MacroblockHeader,
}

impl<'a, R: BitRead> MacroblockResidual<'a, R> {
    pub fn header(&self) -> &MacroblockHeader {
        &self.header
    }

    pub fn residual_block_cavlc(
        &mut self,
        coeff_level: &mut [i32],
        start_idx: usize,
        end_idx: usize,
        max_num_coeff: usize,
        nc: CavlcContext,
    ) -> Result<u8, CavlcError> {
        cavlc::residual_block_cavlc(
            self.core.reader,
            coeff_level,
            start_idx,
            end_idx,
            max_num_coeff,
            nc,
        )
    }

    pub fn curr_mb_addr(&self) -> u32 {
        self.core.curr_mb_addr
    }

    pub fn finish(mut self) -> Result<SliceDataReader<'a, R>, SliceDataError> {
        match self.core.reader.has_more_rbsp_data("slice_data") {
            Ok(false) => {
                self.core.done = true;
            }
            Ok(true) => {}
            Err(_) => {
                self.core.done = true;
            }
        }
        Ok(SliceDataReader { core: self.core })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::pps::SliceGroup;
    use crate::nal::rbsp::{BitReader, BitWrite, BitWriter};
    use crate::nal::sps::{
        ChromaFormat, ChromaInfo, ConstraintFlags, FrameMbsFlags, PicOrderCntType, Profile,
        ProfileIdc, SeqParamSetId, SeqParameterSet,
    };
    use std::io::Cursor;

    fn sps(width_mbs: u32, height_mbs: u32) -> SeqParameterSet {
        SeqParameterSet {
            profile_idc: ProfileIdc::from(Profile::Baseline.profile_idc()),
            constraint_flags: ConstraintFlags::from(0),
            level_idc: 30,
            seq_parameter_set_id: SeqParamSetId::from_u32(0).unwrap(),
            chroma_info: ChromaInfo {
                chroma_format: ChromaFormat::YUV420,
                ..ChromaInfo::default()
            },
            log2_max_frame_num_minus4: 0,
            pic_order_cnt: PicOrderCntType::TypeTwo,
            max_num_ref_frames: 1,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: width_mbs - 1,
            pic_height_in_map_units_minus1: height_mbs - 1,
            frame_mbs_flags: FrameMbsFlags::Frames,
            direct_8x8_inference_flag: true,
            frame_cropping: None,
            vui_parameters: None,
        }
    }

    /// Build an RBSP byte vector containing the Exp-Golomb `mb_skip_run` value followed
    /// by RBSP trailing bits.
    fn skip_run_rbsp(skip_run: u32) -> Vec<u8> {
        let mut w = BitWriter::new(Vec::new());
        w.write_ue(skip_run).unwrap();
        w.write_rbsp_trailing_bits().unwrap();
        w.into_writer()
    }

    /// Drive a P-slice reader over a stream of pure skip macroblocks and return the
    /// raster-scan addresses it emitted.
    fn read_skip_addresses(group_map: SliceGroupMap, first_mb: u32, skip_run: u32) -> Vec<u32> {
        let rbsp = skip_run_rbsp(skip_run);
        // A leading zero byte stands in for the NAL header skipped by ByteReader.
        let mut nal = vec![0u8];
        nal.extend_from_slice(&rbsp);
        let mut reader = BitReader::new(crate::rbsp::ByteReader::skipping_h264_header(
            Cursor::new(nal),
        ));
        let data = match SliceDataReader::with_group_map(
            &mut reader,
            SliceFamily::P,
            first_mb,
            group_map,
            0,
        ) {
            Ok(d) => d,
            Err(e) => panic!("with_group_map failed: {}", e),
        };
        let mut addrs = Vec::new();
        let mut data = data;
        loop {
            match data.next().unwrap() {
                SliceEvent::End => break,
                SliceEvent::Skip { mb_addr, next } => {
                    addrs.push(mb_addr);
                    data = next;
                }
                SliceEvent::Macroblock(_) => panic!("unexpected coded macroblock"),
            }
        }
        addrs
    }

    #[test]
    fn single_group_keeps_raster_order() {
        let s = sps(4, 4);
        let map = SliceGroupMap::for_picture(&s, None, None).unwrap();
        let addrs = read_skip_addresses(map, 0, 16);
        assert_eq!(addrs, (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn explicit_slice_groups_address_actual_positions() {
        let s = sps(4, 4);
        // Even map units go to group 0, odd ones to group 1.
        let ids: Vec<u32> = (0..16).map(|i| i % 2).collect();
        let sg = SliceGroup::ExplicitAssignment {
            num_slice_groups_minus1: 1,
            slice_group_id: ids,
        };
        let map0 = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        let map1 = map0.clone();

        // A slice per group, starting at each group's first map unit.
        let addrs0 = read_skip_addresses(map0, 0, 8);
        let addrs1 = read_skip_addresses(map1, 1, 8);

        assert_eq!(addrs0, vec![0, 2, 4, 6, 8, 10, 12, 14]);
        assert_eq!(addrs1, vec![1, 3, 5, 7, 9, 11, 13, 15]);

        // The two slices together must restore the full picture distribution.
        let mut all = addrs0.clone();
        all.extend(addrs1);
        let mut sorted = all.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..16).collect::<Vec<_>>());
        assert_eq!(all.len(), 16);
    }

    #[test]
    fn dispersed_group_slices_partition_the_picture() {
        let s = sps(8, 8);
        let sg = SliceGroup::Dispersed {
            num_slice_groups_minus1: 1,
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        let len0 = map.group_len(0);
        let len1 = map.group_len(1);
        assert_eq!(len0 + len1, 64);
        let first_of_group1 = map.addr_in_group(1, 0).unwrap();

        let addrs0 =
            read_skip_addresses(map.clone(), map.addr_in_group(0, 0).unwrap(), len0 as u32);
        let addrs1 = read_skip_addresses(map, first_of_group1, len1 as u32);

        assert_eq!(addrs0.len(), len0);
        assert_eq!(addrs1.len(), len1);
        // No overlap, and full coverage.
        let mut all = addrs0;
        all.extend(addrs1);
        assert_eq!(all.len(), 64);
        let mut sorted = all;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn changing_group_map_is_rebuilt_per_picture() {
        let s = sps(4, 4);
        let sg = SliceGroup::Changing {
            change_type: crate::nal::pps::SliceGroupChangeType::RasterScan,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: false,
            slice_group_change_rate_minus1: 3, // rate 4
        };
        // Picture 1: group 1 holds the first four raster units; picture 2 the first eight.
        let map_pic1 = SliceGroupMap::for_picture(&s, Some(&sg), Some(1)).unwrap();
        let map_pic2 = SliceGroupMap::for_picture(&s, Some(&sg), Some(2)).unwrap();
        assert_eq!(map_pic1.group_len(1), 4);
        assert_eq!(map_pic2.group_len(1), 8);

        // The group-1 slice of picture 2 must visit units 0..=7 in order.
        let addrs = read_skip_addresses(map_pic2.clone(), 0, 8);
        assert_eq!(addrs, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        // And group 0 covers the remaining units.
        let first_g0 = map_pic2.addr_in_group(0, 0).unwrap();
        let addrs_g0 = read_skip_addresses(map_pic2, first_g0, 8);
        assert_eq!(addrs_g0, vec![8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn first_mb_outside_picture_is_error() {
        let s = sps(2, 2);
        let map = SliceGroupMap::for_picture(&s, None, None).unwrap();
        let rbsp = skip_run_rbsp(1);
        let mut nal = vec![0u8];
        nal.extend_from_slice(&rbsp);
        let mut reader = BitReader::new(crate::rbsp::ByteReader::skipping_h264_header(
            Cursor::new(nal),
        ));
        let err = match SliceDataReader::with_group_map(&mut reader, SliceFamily::P, 16, map, 0) {
            Ok(_) => panic!("expected an error for an out-of-picture first_mb_in_slice"),
            Err(e) => e,
        };
        assert!(matches!(err, SliceDataError::InvalidFirstMbInSlice(16)));
    }

    #[test]
    fn interleaved_group_slices_partition_picture() {
        let s = sps(4, 4);
        // Runs of 4: group 0 = units 0..4 and 8..12, group 1 = the rest.
        let sg = SliceGroup::Interleaved {
            run_length_minus1: vec![3, 3],
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        let len0 = map.group_len(0) as u32;
        let len1 = map.group_len(1) as u32;
        let addrs0 = read_skip_addresses(map.clone(), 0, len0);
        let first_g1 = map.addr_in_group(1, 0).unwrap();
        let addrs1 = read_skip_addresses(map, first_g1, len1);
        let mut all = addrs0;
        all.extend(addrs1);
        assert_eq!(all.len(), 16);
        let mut sorted = all;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..16).collect::<Vec<_>>());
    }
}
