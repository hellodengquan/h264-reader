//! Tests for the picture order count deriver (spec 8.2.1).
//!
//! SPS/PPS/slice-header values are constructed directly rather than encoded, since the
//! deriver only consumes the parsed structures.

use super::*;
use crate::nal::pps::{PicParamSetId, PicParameterSet};
use crate::nal::slice::{
    DecRefPicMarking, Field, FieldPic, MemoryManagementControlOperation, PicOrderCountLsb,
    RefPicListModifications, SliceExclusive, SliceFamily, SliceHeader, SliceType,
};
use crate::nal::sps::{
    ChromaFormat, ChromaInfo, ConstraintFlags, FrameMbsFlags, PicOrderCntType, ProfileIdc,
    SeqParamSetId, SeqParameterSet,
};

fn sps(poc: PicOrderCntType, fields: bool) -> SeqParameterSet {
    SeqParameterSet {
        profile_idc: ProfileIdc::from(100),
        constraint_flags: ConstraintFlags::from(0),
        level_idc: 30,
        seq_parameter_set_id: SeqParamSetId::from_u32(0).unwrap(),
        chroma_info: ChromaInfo {
            chroma_format: ChromaFormat::YUV420,
            ..ChromaInfo::default()
        },
        log2_max_frame_num_minus4: 0, // MaxFrameNum == 16
        pic_order_cnt: poc,
        max_num_ref_frames: 4,
        gaps_in_frame_num_value_allowed_flag: false,
        pic_width_in_mbs_minus1: 0,
        pic_height_in_map_units_minus1: 0,
        frame_mbs_flags: if fields {
            FrameMbsFlags::Fields {
                mb_adaptive_frame_field_flag: false,
            }
        } else {
            FrameMbsFlags::Frames
        },
        direct_8x8_inference_flag: true,
        frame_cropping: None,
        vui_parameters: None,
    }
}

fn pps(bottom_field_flag: bool) -> PicParameterSet {
    PicParameterSet {
        pic_parameter_set_id: PicParamSetId::from_u32(0).unwrap(),
        seq_parameter_set_id: SeqParamSetId::from_u32(0).unwrap(),
        entropy_coding_mode_flag: false,
        bottom_field_pic_order_in_frame_present_flag: bottom_field_flag,
        slice_groups: None,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        weighted_pred_flag: false,
        weighted_bipred_idc: 0,
        pic_init_qp_minus26: 0,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        deblocking_filter_control_present_flag: false,
        constrained_intra_pred_flag: false,
        redundant_pic_cnt_present_flag: false,
        extension: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn slice_header(
    frame_num: u16,
    field_pic: FieldPic,
    idr: bool,
    nal_ref_idc: u8,
    pic_order_cnt_lsb: Option<PicOrderCountLsb>,
    dec_ref_pic_marking: Option<DecRefPicMarking>,
) -> SliceHeader {
    SliceHeader {
        first_mb_in_slice: 0,
        slice_type: SliceType {
            family: if idr { SliceFamily::I } else { SliceFamily::P },
            exclusive: SliceExclusive::Exclusive,
        },
        colour_plane: None,
        frame_num,
        nal_ref_idc,
        field_pic,
        idr_pic_id: if idr { Some(0) } else { None },
        pic_order_cnt_lsb,
        redundant_pic_cnt: None,
        direct_spatial_mv_pred_flag: None,
        num_ref_idx_active: None,
        ref_pic_list_modification: Some(RefPicListModifications::I),
        pred_weight_table: None,
        dec_ref_pic_marking,
        cabac_init_idc: None,
        slice_qp_delta: 0,
        sp_for_switch_flag: None,
        slice_qs: None,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: None,
        slice_beta_offset_div2: None,
        slice_group_change_cycle: None,
    }
}

fn idr_frame(frame_num: u16, lsb: Option<PicOrderCountLsb>) -> SliceHeader {
    slice_header(
        frame_num,
        FieldPic::Frame,
        true,
        3,
        lsb,
        Some(DecRefPicMarking::Idr {
            no_output_of_prior_pics_flag: false,
            long_term_reference_flag: false,
        }),
    )
}

fn ref_frame(frame_num: u16, lsb: Option<PicOrderCountLsb>) -> SliceHeader {
    slice_header(
        frame_num,
        FieldPic::Frame,
        false,
        3,
        lsb,
        Some(DecRefPicMarking::SlidingWindow),
    )
}

fn non_ref_frame(frame_num: u16, lsb: Option<PicOrderCountLsb>) -> SliceHeader {
    slice_header(frame_num, FieldPic::Frame, false, 0, lsb, None)
}

fn field(
    frame_num: u16,
    f: Field,
    idr: bool,
    lsb: Option<PicOrderCountLsb>,
) -> SliceHeader {
    slice_header(
        frame_num,
        FieldPic::Field(f),
        idr,
        3,
        lsb,
        if idr {
            Some(DecRefPicMarking::Idr {
                no_output_of_prior_pics_flag: false,
                long_term_reference_flag: false,
            })
        } else {
            Some(DecRefPicMarking::SlidingWindow)
        },
    )
}

// ---------- pic_order_cnt_type 0 (progressive, frames) ----------

#[test]
fn type_zero_frames_basic() {
    // log2_max_pic_order_cnt_lsb_minus4 == 0 → MaxPicOrderCntLsb == 16.
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    let r0 = poc
        .record_picture(&idr_frame(0, Some(PicOrderCountLsb::Frame(0))), &sps, &pps)
        .unwrap();
    assert_eq!(
        r0,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 0,
        }
    );

    let r1 = poc
        .record_picture(&ref_frame(1, Some(PicOrderCountLsb::Frame(2))), &sps, &pps)
        .unwrap();
    assert_eq!(
        r1,
        PicOrder::Frame {
            top_field_pic_order_cnt: 2,
            bottom_field_pic_order_cnt: 2,
        }
    );

    let r2 = poc
        .record_picture(&ref_frame(2, Some(PicOrderCountLsb::Frame(4))), &sps, &pps)
        .unwrap();
    assert_eq!(
        r2,
        PicOrder::Frame {
            top_field_pic_order_cnt: 4,
            bottom_field_pic_order_cnt: 4,
        }
    );
}

#[test]
fn type_zero_lsb_wrap_around() {
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(
        &idr_frame(0, Some(PicOrderCountLsb::Frame(14))),
        &sps,
        &pps,
    )
    .unwrap();
    // IDR resets the MSB regardless of its LSB value, so absolute POC starts at 14.
    let r = poc
        .record_picture(
            &ref_frame(1, Some(PicOrderCountLsb::Frame(0))),
            &sps,
            &pps,
        )
        .unwrap();
    // 14 -> 0 with difference 14 >= MaxPicOrderCntLsb/2 (8): MSB gains 16.
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 16,
            bottom_field_pic_order_cnt: 16,
        }
    );
}

#[test]
fn type_zero_frame_num_wrap() {
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, Some(PicOrderCountLsb::Frame(0))), &sps, &pps)
        .unwrap();
    // Drive up to frame_num 15 (MaxFrameNum == 16), then wrap to 0.
    let mut last: i64 = 0;
    for fn_ in 1u16..=15 {
        last += 2;
        let r = poc
            .record_picture(
                &ref_frame(fn_, Some(PicOrderCountLsb::Frame((last % 16) as u32))),
                &sps,
                &pps,
            )
            .unwrap();
        assert_eq!(r.top_field_pic_order_cnt().unwrap(), last);
    }
    // Next picture wraps frame_num; LSB also wraps (32 % 16 == 0).
    last += 2;
    let r = poc
        .record_picture(
            &ref_frame(0, Some(PicOrderCountLsb::Frame(0))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), last);
}

#[test]
fn type_zero_bottom_delta() {
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        true,
    );
    let pps = pps(true);
    let mut poc = PicOrderCounter::new();
    let r = poc
        .record_picture(
            &idr_frame(
                0,
                Some(PicOrderCountLsb::FieldsAbsolute {
                    pic_order_cnt_lsb: 0,
                    delta_pic_order_cnt_bottom: 1,
                }),
            ),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 1,
        }
    );
    let r = poc
        .record_picture(
            &ref_frame(
                1,
                Some(PicOrderCountLsb::FieldsAbsolute {
                    pic_order_cnt_lsb: 2,
                    delta_pic_order_cnt_bottom: -1,
                }),
            ),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 2,
            bottom_field_pic_order_cnt: 1,
        }
    );
}

#[test]
fn type_zero_field_pictures() {
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        true,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    // IDR top field.
    let r = poc
        .record_picture(
            &field(0, Field::Top, true, Some(PicOrderCountLsb::Frame(0))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 0,
        }
    );
    // Following bottom field (same frame_num): bottom = prevTop + ownTopCount.
    let r = poc
        .record_picture(
            &field(0, Field::Bottom, false, Some(PicOrderCountLsb::Frame(2))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Bottom,
            pic_order_cnt: 2,
        }
    );
    // Next top field, frame_num 1.
    let r = poc
        .record_picture(
            &field(1, Field::Top, false, Some(PicOrderCountLsb::Frame(4))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 4,
        }
    );
}

#[test]
fn type_zero_bottom_without_top_is_error() {
    let sps = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        true,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(
        &field(0, Field::Top, true, Some(PicOrderCountLsb::Frame(0))),
        &sps,
        &pps,
    )
    .unwrap();
    let e = poc
        .record_picture(
            &field(2, Field::Bottom, false, Some(PicOrderCountLsb::Frame(2))),
            &sps,
            &pps,
        )
        .unwrap_err();
    assert!(matches!(e, PicOrderCntError::MissingPreviousTopField));
}

// ---------- pic_order_cnt_type 2 (progressive, frames) ----------

#[test]
fn type_two_frames() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    let r = poc
        .record_picture(&idr_frame(0, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 0,
        }
    );
    let r = poc
        .record_picture(&ref_frame(1, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 2,
            bottom_field_pic_order_cnt: 2,
        }
    );
    // Gap: frame_num jumps 2 -> 4, missing frame 3 contributes an intermediate count.
    let r = poc
        .record_picture(&ref_frame(4, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 8,
            bottom_field_pic_order_cnt: 8,
        }
    );
}

#[test]
fn type_two_field_pictures() {
    let sps = sps(PicOrderCntType::TypeTwo, true);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    let r = poc
        .record_picture(&field(0, Field::Top, true, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 0,
        }
    );
    // Paired bottom field (same frame_num): JM/spec bottom formula with zero
    // FrameNumOffset gives the same count as the preceding top field.
    let r = poc
        .record_picture(&field(0, Field::Bottom, false, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Bottom,
            pic_order_cnt: 0,
        }
    );
    let r = poc
        .record_picture(&field(1, Field::Top, false, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 2,
        }
    );
    let r = poc
        .record_picture(&field(1, Field::Bottom, false, None), &sps, &pps)
        .unwrap();
    // Bottom field: preceding top count (2) plus the doubled FrameNumOffset (2 × 2).
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Bottom,
            pic_order_cnt: 6,
        }
    );
}

// ---------- pic_order_cnt_type 1 ----------

#[test]
fn type_one_always_zero() {
    let sps = sps(
        PicOrderCntType::TypeOne {
            delta_pic_order_always_zero_flag: true,
            offset_for_non_ref_pic: -2_147_483_648,
            offset_for_top_to_bottom_field: -2_147_483_648,
            offsets_for_ref_frame: vec![0],
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, Some(PicOrderCountLsb::FieldsDelta([0, 0]))), &sps, &pps)
        .unwrap();
    let r = poc
        .record_picture(
            &ref_frame(1, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    // ref frame offset 0 per position → counts stay at zero.
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 0,
        }
    );
}

#[test]
fn type_one_cycle_and_non_ref() {
    // One ref-frame offset per cycle: [2], i.e. each reference frame advances POC by 2.
    let sps = sps(
        PicOrderCntType::TypeOne {
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: -1,
            offset_for_top_to_bottom_field: 0,
            offsets_for_ref_frame: vec![2],
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    let r = poc
        .record_picture(&idr_frame(0, Some(PicOrderCountLsb::FieldsDelta([0, 0]))), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 0,
        }
    );
    let r = poc
        .record_picture(
            &ref_frame(1, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 2);

    // Non-reference picture at frame_num 2. FrameNumOffset counts all frame positions
    // since the previous reference picture (one position × 2), then the non-ref offset
    // (-1) and delta0 (+1) are applied: 4 - 1 + 1 = 4.
    let r = poc
        .record_picture(
            &non_ref_frame(2, Some(PicOrderCountLsb::FieldsDelta([1, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 4);
    // Non-reference pictures do not alter state: the next reference continues from the
    // state at frame_num 1, counting frame positions 2 and 3 (2 × 2 = 4) → 6.
    let r = poc
        .record_picture(
            &ref_frame(3, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 6);
}

#[test]
fn type_one_multi_offset_cycle() {
    // Cycle of two offsets [2, 4], repeating every two reference frame positions.
    let sps = sps(
        PicOrderCntType::TypeOne {
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offsets_for_ref_frame: vec![2, 4],
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, Some(PicOrderCountLsb::FieldsDelta([0, 0]))), &sps, &pps)
        .unwrap();
    // frame 1: one frame position contributes offset[0] = 2
    let r = poc
        .record_picture(
            &ref_frame(1, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 2);
    // frame 2: another frame position contributes offset[1] = 4
    let r = poc
        .record_picture(
            &ref_frame(2, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 6);
    // frame 3: the cycle restarts at offset[0]; total 2+4+2 = 8
    let r = poc
        .record_picture(
            &ref_frame(3, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 8);
    // frame 4: offset[1] again; total 2+4+2+4 = 12
    let r = poc
        .record_picture(
            &ref_frame(4, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 12);
}

#[test]
fn type_one_zero_cycle_with_gap_is_error() {
    let sps = sps(
        PicOrderCntType::TypeOne {
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offsets_for_ref_frame: Vec::new(),
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, Some(PicOrderCountLsb::FieldsDelta([0, 0]))), &sps, &pps)
        .unwrap();
    // A frame_num gap cannot be filled when the ref-frame cycle is empty.
    let e = poc
        .record_picture(
            &ref_frame(3, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap_err();
    assert!(matches!(e, PicOrderCntError::InvalidPicOrderCntCycle));
}

#[test]
fn type_one_field_pictures() {
    let sps = sps(
        PicOrderCntType::TypeOne {
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 1,
            offsets_for_ref_frame: vec![2],
        },
        true,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();

    // IDR top field: delta0 = 0.
    let r = poc
        .record_picture(
            &field(0, Field::Top, true, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 0,
        }
    );
    // Paired bottom field: prevTop + 2*FNO(0) + 2*TTB(1) = 2.
    let r = poc
        .record_picture(
            &field(0, Field::Bottom, false, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Bottom,
            pic_order_cnt: 2,
        }
    );
    // Next top field, frame_num 1: FNO = 2.
    let r = poc
        .record_picture(
            &field(1, Field::Top, false, Some(PicOrderCountLsb::FieldsDelta([0, 0]))),
            &sps,
            &pps,
        )
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Field {
            field: FieldOrder::Top,
            pic_order_cnt: 2,
        }
    );
}

// ---------- origin / error handling ----------

#[test]
fn no_idr_origin_is_error() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    let e = poc
        .record_picture(&ref_frame(0, None), &sps, &pps)
        .unwrap_err();
    assert!(matches!(e, PicOrderCntError::NoReferencePoint));
}

#[test]
fn non_first_slice_is_error() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    let mut h = idr_frame(0, None);
    h.first_mb_in_slice = 7;
    let e = poc.record_picture(&h, &sps, &pps).unwrap_err();
    assert!(matches!(e, PicOrderCntError::NotFirstSliceInPicture));
}

#[test]
fn redundant_picture_is_error() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    let mut h = idr_frame(0, None);
    h.redundant_pic_cnt = Some(1);
    let e = poc.record_picture(&h, &sps, &pps).unwrap_err();
    assert!(matches!(e, PicOrderCntError::RedundantCodedPicture));
}

#[test]
fn incompatible_sps_change_is_error() {
    let sps0 = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 0,
        },
        false,
    );
    let sps1 = sps(
        PicOrderCntType::TypeZero {
            log2_max_pic_order_cnt_lsb_minus4: 1,
        },
        false,
    );
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, Some(PicOrderCountLsb::Frame(0))), &sps0, &pps)
        .unwrap();
    poc.record_picture(
        &ref_frame(1, Some(PicOrderCountLsb::Frame(2))),
        &sps0,
        &pps,
    )
    .unwrap();
    // A non-IDR picture whose SPS changes the order-count parameters is incompatible
    // with the state anchored at the previous IDR.
    let e = poc
        .record_picture(
            &ref_frame(2, Some(PicOrderCountLsb::Frame(4))),
            &sps1,
            &pps,
        )
        .unwrap_err();
    assert!(matches!(
        e,
        PicOrderCntError::IncompatibleSequenceParameters(_)
    ));

    // A new IDR carrying the new parameters re-establishes the origin successfully.
    poc.record_picture(
        &idr_frame(0, Some(PicOrderCountLsb::Frame(0))),
        &sps1,
        &pps,
    )
    .unwrap();
    // The new parameter regime is now active; the old SPS is the incompatible one.
    let e = poc
        .record_picture(
            &ref_frame(1, Some(PicOrderCountLsb::Frame(2))),
            &sps0,
            &pps,
        )
        .unwrap_err();
    assert!(matches!(
        e,
        PicOrderCntError::IncompatibleSequenceParameters(_)
    ));
}

#[test]
fn idr_restart_from_any_idr_matches_segmented_feed() {
    // A long stream with a mid-stream IDR: feeding the tail (starting at that IDR) into
    // a fresh counter must produce exactly the same per-picture order counts as feeding
    // the whole stream into one counter. Checked for every pic_order_cnt_type.
    let cases = [
        sps(
            PicOrderCntType::TypeZero {
                log2_max_pic_order_cnt_lsb_minus4: 0,
            },
            false,
        ),
        sps(
            PicOrderCntType::TypeOne {
                delta_pic_order_always_zero_flag: false,
                offset_for_non_ref_pic: 0,
                offset_for_top_to_bottom_field: 0,
                offsets_for_ref_frame: vec![2],
            },
            false,
        ),
        sps(PicOrderCntType::TypeTwo, false),
    ];
    let pps = pps(false);

    for sps in cases {
        let lsb = |v: u32| match &sps.pic_order_cnt {
            PicOrderCntType::TypeZero { .. } => Some(PicOrderCountLsb::Frame(v)),
            PicOrderCntType::TypeOne { .. } => {
                Some(PicOrderCountLsb::FieldsDelta([v as i32, 0]))
            }
            PicOrderCntType::TypeTwo => None,
        };
        let pictures: Vec<SliceHeader> = vec![
            idr_frame(0, lsb(0)),
            ref_frame(1, lsb(2)),
            ref_frame(2, lsb(4)),
            idr_frame(0, lsb(0)),
            ref_frame(1, lsb(2)),
            ref_frame(2, lsb(4)),
        ];

        let mut full = PicOrderCounter::new();
        let full_results: Vec<PicOrder> = pictures
            .iter()
            .map(|h| full.record_picture(h, &sps, &pps).unwrap())
            .collect();

        // Tail starting at the second IDR (index 3).
        let mut segmented = PicOrderCounter::new();
        for (i, h) in pictures[3..].iter().enumerate() {
            let r = segmented.record_picture(h, &sps, &pps).unwrap();
            assert_eq!(r, full_results[3 + i]);
        }

        // A counter fed only the very first IDR must produce the same origin value.
        let mut only_origin = PicOrderCounter::new();
        let first = only_origin
            .record_picture(&pictures[0], &sps, &pps)
            .unwrap();
        assert_eq!(first, full_results[0]);
    }
}

#[test]
fn mmco_operation_five_resets_origin() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, None), &sps, &pps).unwrap();
    poc.record_picture(&ref_frame(1, None), &sps, &pps).unwrap();
    assert_eq!(
        poc.record_picture(&ref_frame(2, None), &sps, &pps).unwrap(),
        PicOrder::Frame {
            top_field_pic_order_cnt: 4,
            bottom_field_pic_order_cnt: 4,
        }
    );

    // MMCO operation 5 marks all reference pictures unused and resets POC state, even
    // though the picture is not an IDR.
    let mut h = ref_frame(3, None);
    h.dec_ref_pic_marking = Some(DecRefPicMarking::Adaptive(vec![
        MemoryManagementControlOperation::AllRefPicturesUnused,
    ]));
    let r = poc.record_picture(&h, &sps, &pps).unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 0,
            bottom_field_pic_order_cnt: 0,
        }
    );
    // Order counts restart from the new origin.
    let r = poc
        .record_picture(&ref_frame(4, None), &sps, &pps)
        .unwrap();
    assert_eq!(
        r,
        PicOrder::Frame {
            top_field_pic_order_cnt: 2,
            bottom_field_pic_order_cnt: 2,
        }
    );
}

#[test]
fn type_two_frame_num_wrap_around() {
    let sps = sps(PicOrderCntType::TypeTwo, false);
    let pps = pps(false);
    let mut poc = PicOrderCounter::new();
    poc.record_picture(&idr_frame(0, None), &sps, &pps).unwrap();
    for fn_ in 1u16..=15 {
        let r = poc
            .record_picture(&ref_frame(fn_, None), &sps, &pps)
            .unwrap();
        assert_eq!(r.top_field_pic_order_cnt().unwrap(), 2 * i64::from(fn_));
    }
    // Wrapped frame_num keeps the absolute progression rather than restarting.
    let r = poc
        .record_picture(&ref_frame(0, None), &sps, &pps)
        .unwrap();
    assert_eq!(r.top_field_pic_order_cnt().unwrap(), 32);
}
