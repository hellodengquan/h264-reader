//! Slice group map construction (spec section 8.2.2).
//!
//! When a PPS declares more than one slice group, the macroblocks of a picture are not
//! visited in simple raster order. Instead, every _map unit_ (one macroblock, for the
//! non-MBAFF streams this crate supports) is assigned to a slice group according to the
//! `slice_group_map_type`, and each slice only covers the map units of its own group.
//!
//! [`SliceGroupMap`] builds the per-picture assignment from the PPS declaration and the
//! SPS picture geometry, and lets [`super::data::SliceDataReader`] translate the
//! incremental macroblock addresses of a slice into actual raster-scan positions on the
//! picture. Map types 3–5 change from picture to picture, so a fresh `SliceGroupMap` must
//! be built per picture using the `slice_group_change_cycle` value from the slice header.

use crate::nal::pps::{SliceGroup, SliceGroupChangeType};
use crate::nal::sps::SeqParameterSet;
/// Errors that can occur while building a [`SliceGroupMap`] from a PPS slice-group
/// declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceGroupMapError {
    /// A map type 0 (interleaved) PPS declared run lengths that don't together cover
    /// exactly the whole picture.
    IncompleteInterleavedMap { assigned: u32, pic_size: u32 },
    /// A map type 6 (explicit assignment) PPS declared a `slice_group_id` outside the
    /// range `0..=num_slice_groups_minus1`.
    InvalidSliceGroupId(u32),
    /// The PPS's explicit `slice_group_id` list does not cover every map unit of the
    /// picture.
    IncompleteExplicitMap { assigned: u32, pic_size: u32 },
    /// A map type 2 rectangle's `top_left`/`bottom_right` indexes fell outside the
    /// picture.
    RectangleOutOfBounds(u32),
    /// The `slice_group_change_cycle` supplied for a map type 3–5 PPS would make the
    /// size of the upper-left slice group exceed the picture size.
    ChangeCycleTooLarge { change_cycle: u32, pic_size: u32 },
    /// A geometric scan in a map type 3–5 assignment could not be completed (the PPS
    /// declaration is not interpretable for this picture geometry).
    InvalidGeometry,
    /// An arithmetic intermediate value exceeded the range representable in `u32`.
    ArithmeticOverflow,
}

impl std::fmt::Display for SliceGroupMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SliceGroupMapError::IncompleteInterleavedMap { assigned, pic_size } => write!(
                f,
                "interleaved slice group runs cover {assigned} of {pic_size} map units"
            ),
            SliceGroupMapError::InvalidSliceGroupId(id) => {
                write!(f, "slice_group_id {id} names a non-existent slice group")
            }
            SliceGroupMapError::IncompleteExplicitMap { assigned, pic_size } => write!(
                f,
                "explicit slice_group_id covers {assigned} of {pic_size} map units"
            ),
            SliceGroupMapError::RectangleOutOfBounds(idx) => {
                write!(f, "slice group rectangle index {idx} is outside the picture")
            }
            SliceGroupMapError::ChangeCycleTooLarge {
                change_cycle,
                pic_size,
            } => write!(
                f,
                "slice_group_change_cycle {change_cycle} gives an upper-left group larger than the picture ({pic_size} map units)"
            ),
            SliceGroupMapError::InvalidGeometry => {
                write!(f, "slice group geometry scan could not cover the picture")
            }
            SliceGroupMapError::ArithmeticOverflow => {
                write!(f, "arithmetic overflow while building slice group map")
            }
        }
    }
}

impl std::error::Error for SliceGroupMapError {}

/// The assignment of every map unit of one picture to a slice group, together with the
/// per-group address lists used to walk the macroblocks of a slice in FMO order.
#[derive(Debug, Clone)]
pub struct SliceGroupMap {
    /// `map_unit_to_group[i]` is the slice group id of raster-scan map unit `i`.
    map_unit_to_group: Vec<u8>,
    /// `group_addrs[g]` holds the raster-scan map unit addresses belonging to group `g`,
    /// in ascending order.
    group_addrs: Vec<Vec<u32>>,
    pic_size_in_map_units: u32,
}

impl SliceGroupMap {
    /// Build the slice group map for a single picture.
    ///
    /// `slice_group` is the parsed [`SliceGroup`] from the active PPS (or `None` for a
    /// single-slice-group PPS, in which case plain raster order is used). `change_cycle`
    /// must be the `slice_group_change_cycle` value from the current picture's slice
    /// header for map types 3–5, and is ignored otherwise.
    pub fn for_picture(
        sps: &SeqParameterSet,
        slice_group: Option<&SliceGroup>,
        change_cycle: Option<u32>,
    ) -> Result<SliceGroupMap, SliceGroupMapError> {
        let pic_width = sps.pic_width_in_mbs();
        let pic_height = sps.pic_height_in_map_units();
        let pic_size = sps.pic_size_in_map_units();

        let Some(slice_group) = slice_group else {
            return Ok(Self::identity(pic_size));
        };

        let num_groups = match slice_group {
            SliceGroup::Interleaved { run_length_minus1 } => run_length_minus1.len(),
            SliceGroup::Dispersed {
                num_slice_groups_minus1,
            }
            | SliceGroup::Changing {
                num_slice_groups_minus1,
                ..
            }
            | SliceGroup::ExplicitAssignment {
                num_slice_groups_minus1,
                ..
            } => *num_slice_groups_minus1 as usize + 1,
            SliceGroup::ForegroundAndLeftover { rectangles } => rectangles.len() + 1,
        };

        let mut map_unit_to_group = vec![0u8; pic_size as usize];
        match slice_group {
            SliceGroup::Interleaved { run_length_minus1 } => {
                build_interleaved(&mut map_unit_to_group, run_length_minus1, pic_size)?;
            }
            SliceGroup::Dispersed { .. } => {
                build_dispersed(&mut map_unit_to_group, num_groups, pic_width, pic_height);
            }
            SliceGroup::ForegroundAndLeftover { rectangles } => {
                build_foreground(
                    &mut map_unit_to_group,
                    rectangles,
                    pic_width,
                    pic_height,
                    pic_size,
                )?;
            }
            SliceGroup::Changing {
                change_type,
                slice_group_change_direction_flag,
                slice_group_change_rate_minus1,
                ..
            } => {
                let change_cycle = change_cycle.unwrap_or(0);
                build_changing(
                    &mut map_unit_to_group,
                    change_type,
                    *slice_group_change_direction_flag,
                    slice_group_change_rate_minus1 + 1,
                    change_cycle,
                    pic_width,
                    pic_height,
                    pic_size,
                )?;
            }
            SliceGroup::ExplicitAssignment { slice_group_id, .. } => {
                build_explicit(&mut map_unit_to_group, slice_group_id, num_groups, pic_size)?;
            }
        }

        Ok(Self::from_group_map(
            map_unit_to_group,
            num_groups,
            pic_size,
        ))
    }

    /// Build the slice group map for a single picture directly from the active
    /// parameter sets.
    ///
    /// This is the typical entry point: pass the SPS and PPS that the slice header
    /// resolved to, together with the parsed
    /// [`slice_group_change_cycle`](crate::nal::slice::SliceHeader::slice_group_change_cycle)
    /// from that header. A single-slice-group PPS yields plain raster order; map types
    /// 3–5 use `change_cycle` to produce the map for this specific picture.
    pub fn from_parameter_sets(
        sps: &SeqParameterSet,
        pps: &crate::nal::pps::PicParameterSet,
        change_cycle: Option<u32>,
    ) -> Result<SliceGroupMap, SliceGroupMapError> {
        Self::for_picture(sps, pps.slice_groups.as_ref(), change_cycle)
    }

    /// Construct the identity map used by ordinary single-slice-group pictures: every
    /// map unit belongs to group 0 and addresses advance in raster order.
    pub fn single_group(pic_size_in_map_units: u32) -> SliceGroupMap {
        Self::identity(pic_size_in_map_units)
    }

    fn identity(pic_size: u32) -> SliceGroupMap {
        let map_unit_to_group = vec![0u8; pic_size as usize];
        Self::from_group_map(map_unit_to_group, 1, pic_size)
    }

    fn from_group_map(
        map_unit_to_group: Vec<u8>,
        num_groups: usize,
        pic_size: u32,
    ) -> SliceGroupMap {
        let mut group_addrs = vec![Vec::new(); num_groups];
        for (addr, &group) in map_unit_to_group.iter().enumerate() {
            group_addrs[group as usize].push(addr as u32);
        }
        SliceGroupMap {
            map_unit_to_group,
            group_addrs,
            pic_size_in_map_units: pic_size,
        }
    }

    /// Number of map units (macroblocks, without MBAFF) in the picture.
    pub fn pic_size_in_map_units(&self) -> u32 {
        self.pic_size_in_map_units
    }

    /// Slice group id of the given raster-scan map unit address.
    pub fn group_of(&self, map_unit_addr: u32) -> Option<u8> {
        self.map_unit_to_group.get(map_unit_addr as usize).copied()
    }

    /// Raster-scan map unit address at `index` within slice group `group`.
    ///
    /// Returns `None` once every map unit of the group has been visited.
    pub(crate) fn addr_in_group(&self, group: u8, index: usize) -> Option<u32> {
        self.group_addrs
            .get(group as usize)
            .and_then(|addrs| addrs.get(index).copied())
    }

    /// Position of `map_unit_addr` within its group's ascending address list, together
    /// with the group id. Used to position a reader at `first_mb_in_slice`.
    pub(crate) fn group_position(&self, map_unit_addr: u32) -> Option<(u8, usize)> {
        let group = self.group_of(map_unit_addr)?;
        let index = self.group_addrs[group as usize]
            .iter()
            .position(|&addr| addr == map_unit_addr)?;
        Some((group, index))
    }

    pub(crate) fn group_len(&self, group: u8) -> usize {
        self.group_addrs[group as usize].len()
    }
}

#[inline]
fn mb_addr(x: u32, y: u32, pic_width: u32) -> Result<u32, SliceGroupMapError> {
    y.checked_mul(pic_width)
        .and_then(|row| row.checked_add(x))
        .ok_or(SliceGroupMapError::ArithmeticOverflow)
}

/// Map type 0: interleaved runs of whole slice groups.
fn build_interleaved(
    map: &mut [u8],
    run_length_minus1: &[u32],
    pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    let mut i = 0u32;
    for (group, run_minus1) in run_length_minus1.iter().enumerate().cycle() {
        let run = run_minus1 + 1;
        for _ in 0..run {
            if i >= pic_size {
                // The runs overran the picture; the PPS declaration is inconsistent.
                return Err(SliceGroupMapError::IncompleteInterleavedMap {
                    assigned: i,
                    pic_size,
                });
            }
            map[i as usize] = group as u8;
            i += 1;
        }
        if i == pic_size {
            return Ok(());
        }
    }
    // `cycle()` on a non-empty slice never returns otherwise.
    Ok(())
}

/// Map type 1: dispersed (checkerboard-like) assignment.
fn build_dispersed(map: &mut [u8], num_groups: usize, pic_width: u32, pic_height: u32) {
    let groups = num_groups as u32;
    for y in 0..pic_height {
        for x in 0..pic_width {
            let group =
                ((x + y) % groups + ((((y / 2) % groups) + (x / 2)) % groups) * groups) % groups;
            map[(y * pic_width + x) as usize] = group as u8;
        }
    }
}

/// Map type 2: one or more foreground rectangles, with everything else left to the final
/// slice group.
fn build_foreground(
    map: &mut [u8],
    rectangles: &[crate::nal::pps::SliceRect],
    pic_width: u32,
    pic_height: u32,
    pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    // The leftover group is the id after all the foreground groups; everything not
    // covered by a rectangle belongs to it.
    let leftover_group = rectangles.len() as u8;
    for slot in map.iter_mut() {
        *slot = leftover_group;
    }
    for (group, rect) in rectangles.iter().enumerate() {
        let (top_left, bottom_right) = rect.addrs();
        if bottom_right >= pic_size {
            return Err(SliceGroupMapError::RectangleOutOfBounds(bottom_right));
        }
        let x_left = top_left % pic_width;
        let x_right = bottom_right % pic_width;
        let y_top = top_left / pic_width;
        let y_bottom = bottom_right / pic_width;
        if y_bottom >= pic_height {
            return Err(SliceGroupMapError::RectangleOutOfBounds(bottom_right));
        }
        for y in y_top..=y_bottom {
            for x in x_left..=x_right {
                map[mb_addr(x, y, pic_width)? as usize] = group as u8;
            }
        }
    }
    Ok(())
}

/// Map type 6: explicit per-map-unit group assignment.
fn build_explicit(
    map: &mut [u8],
    slice_group_id: &[u32],
    num_groups: usize,
    pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    if slice_group_id.len() < pic_size as usize {
        return Err(SliceGroupMapError::IncompleteExplicitMap {
            assigned: slice_group_id.len() as u32,
            pic_size,
        });
    }
    for (i, &group) in slice_group_id.iter().enumerate() {
        if group >= num_groups as u32 {
            return Err(SliceGroupMapError::InvalidSliceGroupId(group));
        }
        map[i] = group as u8;
    }
    Ok(())
}

/// Map types 3–5: a growing/shrinking region whose size changes every picture.
#[allow(clippy::too_many_arguments)]
fn build_changing(
    map: &mut [u8],
    change_type: &SliceGroupChangeType,
    change_direction_flag: bool,
    change_rate: u32,
    change_cycle: u32,
    pic_width: u32,
    pic_height: u32,
    pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    // SizeOfUpperLeftGroup (spec 8.2.2.3). In the reverse direction the region size is
    // the picture size minus the number of units already moved into the other group; if
    // the change cycle would overshoot the picture this is an uninterpretable declaration.
    let size_upper_left = if change_direction_flag {
        pic_size
            .checked_sub(
                change_cycle
                    .checked_mul(change_rate)
                    .ok_or(SliceGroupMapError::ArithmeticOverflow)?,
            )
            .ok_or(SliceGroupMapError::ChangeCycleTooLarge {
                change_cycle,
                pic_size,
            })?
    } else {
        change_cycle
            .checked_mul(change_rate)
            .ok_or(SliceGroupMapError::ArithmeticOverflow)?
    };
    if size_upper_left > pic_size {
        return Err(SliceGroupMapError::ChangeCycleTooLarge {
            change_cycle,
            pic_size,
        });
    }

    // With the reverse direction the "background" group is 1 and the scanned region is
    // assigned group 0; otherwise the background is 0 and the region becomes group 1.
    let (background_group, region_group) = if change_direction_flag {
        (1u8, 0u8)
    } else {
        (0u8, 1u8)
    };
    for slot in map.iter_mut() {
        *slot = background_group;
    }

    // No map units belong to the growing region this picture.
    if size_upper_left == 0 {
        return Ok(());
    }

    match change_type {
        SliceGroupChangeType::RasterScan => {
            // Map type 4: the first SizeOfUpperLeftGroup units of a plain raster scan.
            for addr in 0..size_upper_left {
                map[addr as usize] = region_group;
            }
            Ok(())
        }
        SliceGroupChangeType::BoxOut => box_out(
            map,
            size_upper_left,
            region_group,
            pic_width,
            pic_height,
            pic_size,
        ),
        SliceGroupChangeType::WipeOut => wipe_out(
            map,
            size_upper_left,
            region_group,
            pic_width,
            pic_height,
            pic_size,
        ),
    }
}

/// Map type 3: expand a box outwards from the picture centre until the region contains
/// `size_upper_left` map units.
///
/// A rectangle is grown from the centre one column/row at a time (left column, right
/// column, top row, bottom row, repeated) until it covers the whole picture. Every map
/// unit is assigned exactly once, so the region size always matches the requested
/// `size_upper_left` and the fully-grown box covers the entire picture.
fn box_out(
    map: &mut [u8],
    size_upper_left: u32,
    region_group: u8,
    pic_width: u32,
    pic_height: u32,
    _pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    let mut left = (pic_width - u32::from(pic_width % 2 == 0)) / 2;
    let mut right = left;
    let mut top = (pic_height - u32::from(pic_height % 2 == 0)) / 2;
    let mut bottom = top;
    let mut mapped = 0u32;

    let assign =
        |map: &mut [u8], x: u32, y: u32, mapped: &mut u32| -> Result<bool, SliceGroupMapError> {
            map[mb_addr(x, y, pic_width)? as usize] = region_group;
            *mapped += 1;
            Ok(*mapped >= size_upper_left)
        };

    // Seed the centre unit.
    if assign(map, left, top, &mut mapped)? {
        return Ok(());
    }

    for _ in 0..pic_size_cap(pic_width, pic_height)? {
        // Expand one column to the left.
        if left > 0 {
            left -= 1;
            for y in top..=bottom {
                if assign(map, left, y, &mut mapped)? {
                    return Ok(());
                }
            }
        }
        // Expand one column to the right.
        if right + 1 < pic_width {
            right += 1;
            for y in top..=bottom {
                if assign(map, right, y, &mut mapped)? {
                    return Ok(());
                }
            }
        }
        // Expand one row upwards.
        if top > 0 {
            top -= 1;
            for x in left..=right {
                if assign(map, x, top, &mut mapped)? {
                    return Ok(());
                }
            }
        }
        // Expand one row downwards.
        if bottom + 1 < pic_height {
            bottom += 1;
            for x in left..=right {
                if assign(map, x, bottom, &mut mapped)? {
                    return Ok(());
                }
            }
        }

        if left == 0 && right + 1 == pic_width && top == 0 && bottom + 1 == pic_height {
            break;
        }
    }

    if mapped >= size_upper_left {
        Ok(())
    } else {
        Err(SliceGroupMapError::InvalidGeometry)
    }
}

/// Map type 5: four edge-anchored wipes that together sweep the whole picture.
///
/// The picture is split into four quadrants meeting at the picture centre. A cursor
/// sweeps each quadrant in raster order from its outer corner, and the four cursors are
/// advanced round-robin, so the region grows inward from all four edges/corners at once.
/// Each map unit is visited exactly once; when all four quadrants are exhausted the
/// whole picture has been covered.
fn wipe_out(
    map: &mut [u8],
    size_upper_left: u32,
    region_group: u8,
    pic_width: u32,
    pic_height: u32,
    _pic_size: u32,
) -> Result<(), SliceGroupMapError> {
    let mid_x = pic_width / 2;
    let mid_y = pic_height / 2;
    // Column/row ranges (start, end-exclusive) of the four quadrants.
    let quadrants = [
        (0u32, mid_x, 0u32, mid_y),            // top-left
        (mid_x, pic_width, 0u32, mid_y),       // top-right
        (0u32, mid_x, mid_y, pic_height),      // bottom-left
        (mid_x, pic_width, mid_y, pic_height), // bottom-right
    ];
    // Local cursor (column index, row index) inside each quadrant.
    let mut cursors = [(0u32, 0u32); 4];
    let mut mapped = 0u32;

    for _ in 0..pic_size_cap(pic_width, pic_height)? {
        let mut any_alive = false;
        for (q, &(x0, x1, y0, y1)) in quadrants.iter().enumerate() {
            let (ci, ri) = &mut cursors[q];
            let q_width = x1 - x0;
            let q_height = y1 - y0;
            if *ri >= q_height {
                continue; // this quadrant is fully swept
            }
            any_alive = true;
            let x = x0 + *ci;
            let y = y0 + *ri;
            map[mb_addr(x, y, pic_width)? as usize] = region_group;
            mapped += 1;
            // Advance the quadrant cursor in raster order.
            *ci += 1;
            if *ci >= q_width {
                *ci = 0;
                *ri += 1;
            }
            if mapped >= size_upper_left {
                return Ok(());
            }
        }
        if !any_alive {
            break;
        }
    }

    if mapped >= size_upper_left {
        Ok(())
    } else {
        Err(SliceGroupMapError::InvalidGeometry)
    }
}

/// Upper bound on the number of scan steps needed to cover a picture, used to guarantee
/// termination on malformed geometry.
fn pic_size_cap(pic_width: u32, pic_height: u32) -> Result<u32, SliceGroupMapError> {
    pic_width
        .checked_mul(pic_height)
        .ok_or(SliceGroupMapError::ArithmeticOverflow)
        .map(|size| size.saturating_add(4))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::pps::{SliceGroupChangeType, SliceRect};

    fn sps(width_mbs: u32, height_mbs: u32) -> SeqParameterSet {
        use crate::nal::sps::{
            ChromaFormat, ChromaInfo, FrameMbsFlags, PicOrderCntType, Profile, ProfileIdc,
            SeqParamSetId,
        };
        SeqParameterSet {
            profile_idc: ProfileIdc::from(Profile::Baseline.profile_idc()),
            constraint_flags: crate::nal::sps::ConstraintFlags::from(0),
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

    /// Every map unit must belong to a valid group, and the per-group address lists must
    /// together reproduce every picture address exactly once.
    fn assert_partition(map: &SliceGroupMap, num_groups: usize, pic_size: u32) {
        assert_eq!(map.pic_size_in_map_units(), pic_size);
        let mut seen = vec![false; pic_size as usize];
        for g in 0..num_groups {
            let mut prev = None;
            for &addr in &map.group_addrs[g] {
                assert!(addr < pic_size);
                assert!(!seen[addr as usize], "addr {} assigned twice", addr);
                seen[addr as usize] = true;
                assert_eq!(map.group_of(addr), Some(g as u8));
                if let Some(p) = prev {
                    assert!(addr > p, "group addresses must be ascending");
                }
                prev = Some(addr);
            }
        }
        assert!(seen.into_iter().all(std::convert::identity));
    }

    #[test]
    fn single_group_is_raster_order() {
        let s = sps(4, 4);
        let map = SliceGroupMap::for_picture(&s, None, None).unwrap();
        assert_partition(&map, 1, 16);
        for addr in 0..16 {
            assert_eq!(map.group_of(addr), Some(0));
            assert_eq!(map.addr_in_group(0, addr as usize), Some(addr));
        }
    }

    #[test]
    fn interleaved_map_type_0() {
        let s = sps(4, 4);
        // Two groups with runs of 4 each: rows alternate between groups.
        let sg = SliceGroup::Interleaved {
            run_length_minus1: vec![3, 3],
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        assert_partition(&map, 2, 16);
        for addr in 0..16 {
            assert_eq!(
                map.group_of(addr),
                Some(if addr < 4 || (8..12).contains(&addr) {
                    0
                } else {
                    1
                })
            );
        }
    }

    #[test]
    fn interleaved_runs_not_covering_picture_is_error() {
        let s = sps(4, 4);
        // Runs of 10 and 10 units overrun the 16-unit picture.
        let sg = SliceGroup::Interleaved {
            run_length_minus1: vec![9, 9],
        };
        let err = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap_err();
        assert!(matches!(
            err,
            SliceGroupMapError::IncompleteInterleavedMap { .. }
        ));
    }

    #[test]
    fn dispersed_map_type_1() {
        let s = sps(8, 8);
        let sg = SliceGroup::Dispersed {
            num_slice_groups_minus1: 1,
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        assert_partition(&map, 2, 64);
        // Both groups must be non-empty and the pattern must actually be non-raster.
        assert!(map.group_len(0) > 0 && map.group_len(1) > 0);
        assert_ne!(map.group_of(1), map.group_of(0));
    }

    #[test]
    fn foreground_map_type_2() {
        let s = sps(8, 8);
        // A 2x2 foreground rectangle at columns 2..3, rows 2..3 (map units 18,19,26,27).
        let rect = SliceRect::new_for_test(18, 27);
        let sg = SliceGroup::ForegroundAndLeftover {
            rectangles: vec![rect],
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        assert_partition(&map, 2, 64);
        // The foreground rectangle is group 0 (4 units); everything outside it is the
        // leftover group 1.
        assert_eq!(map.group_len(0), 4);
        assert_eq!(map.group_len(1), 60);
        for addr in [18u32, 19, 26, 27] {
            assert_eq!(map.group_of(addr), Some(0));
        }
        assert_eq!(map.group_of(0), Some(1));
    }

    #[test]
    fn explicit_map_type_6() {
        let s = sps(4, 2);
        let ids: Vec<u32> = (0..8).map(|i| i % 2).collect();
        let sg = SliceGroup::ExplicitAssignment {
            num_slice_groups_minus1: 1,
            slice_group_id: ids,
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap();
        assert_partition(&map, 2, 8);
        assert_eq!(map.group_len(0), 4);
        assert_eq!(map.group_len(1), 4);
    }

    #[test]
    fn explicit_invalid_group_id_is_error() {
        let s = sps(2, 2);
        let sg = SliceGroup::ExplicitAssignment {
            num_slice_groups_minus1: 1,
            slice_group_id: vec![0, 0, 0, 5],
        };
        let err = SliceGroupMap::for_picture(&s, Some(&sg), None).unwrap_err();
        assert!(matches!(err, SliceGroupMapError::InvalidSliceGroupId(5)));
    }

    #[test]
    fn changing_raster_scan_type_4_grows_per_picture() {
        let s = sps(4, 4);
        let sg = SliceGroup::Changing {
            change_type: SliceGroupChangeType::RasterScan,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: false,
            slice_group_change_rate_minus1: 3, // rate = 4 map units per cycle
        };
        // Picture 1: 4 units in group 1; picture 2: 8 units.
        let map1 = SliceGroupMap::for_picture(&s, Some(&sg), Some(1)).unwrap();
        let map2 = SliceGroupMap::for_picture(&s, Some(&sg), Some(2)).unwrap();
        assert_partition(&map1, 2, 16);
        assert_partition(&map2, 2, 16);
        assert_eq!(map1.group_len(1), 4);
        assert_eq!(map2.group_len(1), 8);
        assert_eq!(map1.group_of(0), Some(1));
        assert_eq!(map1.group_of(4), Some(0));
        assert_eq!(map2.group_of(7), Some(1));
    }

    #[test]
    fn changing_box_out_type_3_partitions_picture() {
        let s = sps(8, 8);
        let sg = SliceGroup::Changing {
            change_type: SliceGroupChangeType::BoxOut,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: false,
            slice_group_change_rate_minus1: 0, // rate 1
        };
        // Every map unit is assigned exactly once, so with change rate 1 the region size
        // grows by exactly one unit per cycle and eventually covers the whole picture.
        let mut previous = 0usize;
        for cycle in 1..=64 {
            let map = SliceGroupMap::for_picture(&s, Some(&sg), Some(cycle)).unwrap();
            assert_partition(&map, 2, 64);
            assert_eq!(map.group_len(1) as u32, cycle);
            assert!(map.group_len(1) >= previous);
            previous = map.group_len(1);
        }
        let full = SliceGroupMap::for_picture(&s, Some(&sg), Some(64)).unwrap();
        assert_eq!(full.group_len(1), 64);
    }

    #[test]
    fn changing_wipe_out_type_5_partitions_picture() {
        let s = sps(8, 8);
        let sg = SliceGroup::Changing {
            change_type: SliceGroupChangeType::WipeOut,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: false,
            slice_group_change_rate_minus1: 0, // rate 1
        };
        // The four edge wipes together cover the whole picture; every cycle must give a
        // valid partition with the region growing monotonically.
        let mut previous = 0usize;
        for cycle in [1u32, 4, 8, 16, 32, 64] {
            let map = SliceGroupMap::for_picture(&s, Some(&sg), Some(cycle)).unwrap();
            assert_partition(&map, 2, 64);
            assert!(map.group_len(1) >= previous);
            previous = map.group_len(1);
        }
        let full = SliceGroupMap::for_picture(&s, Some(&sg), Some(64)).unwrap();
        assert_eq!(full.group_len(1), 64);
    }

    #[test]
    fn changing_reverse_direction_shrinks_region() {
        let s = sps(4, 4);
        let sg = SliceGroup::Changing {
            change_type: SliceGroupChangeType::RasterScan,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: true,
            slice_group_change_rate_minus1: 3, // rate 4
        };
        let map = SliceGroupMap::for_picture(&s, Some(&sg), Some(2)).unwrap();
        assert_partition(&map, 2, 16);
        // Upper-left group size = 16 - 2*4 = 8, assigned group 0; the rest is group 1.
        assert_eq!(map.group_len(0), 8);
        assert_eq!(map.group_len(1), 8);
    }

    #[test]
    fn change_cycle_too_large_is_error() {
        let s = sps(2, 2);
        let sg = SliceGroup::Changing {
            change_type: SliceGroupChangeType::RasterScan,
            num_slice_groups_minus1: 1,
            slice_group_change_direction_flag: false,
            slice_group_change_rate_minus1: 3, // rate 4
        };
        let err = SliceGroupMap::for_picture(&s, Some(&sg), Some(2)).unwrap_err();
        assert!(matches!(
            err,
            SliceGroupMapError::ChangeCycleTooLarge { .. }
        ));
    }

    /// Exhaustively check map types 3–5 over several geometries: every legal change
    /// cycle must produce a valid two-group partition whose region size matches the
    /// spec's SizeOfUpperLeftGroup, and growth must be monotonic across pictures.
    #[test]
    fn changing_maps_are_valid_for_every_geometry_and_cycle() {
        let geometries = [(2u32, 2u32), (3, 5), (7, 4), (8, 8), (16, 9), (13, 11)];
        let change_types = [
            SliceGroupChangeType::BoxOut,
            SliceGroupChangeType::RasterScan,
            SliceGroupChangeType::WipeOut,
        ];
        for &(w, h) in &geometries {
            let s = sps(w, h);
            let pic_size = w * h;
            for change_type in &change_types {
                for direction in [false, true] {
                    let sg = SliceGroup::Changing {
                        change_type: change_type.clone(),
                        num_slice_groups_minus1: 1,
                        slice_group_change_direction_flag: direction,
                        slice_group_change_rate_minus1: 0, // rate 1
                    };
                    let mut previous_region: Option<usize> = None;
                    for cycle in 0..=pic_size {
                        let map = SliceGroupMap::for_picture(&s, Some(&sg), Some(cycle))
                            .unwrap_or_else(|e| {
                                panic!(
                                    "{:?} dir={} {}x{} cycle={}: {e:?}",
                                    change_type, direction, w, h, cycle
                                )
                            });
                        assert_partition(&map, 2, pic_size);
                        let expected = if direction { pic_size - cycle } else { cycle };
                        let region = if direction {
                            map.group_len(0)
                        } else {
                            map.group_len(1)
                        };
                        assert_eq!(
                            region as u32, expected,
                            "{:?} dir={} {}x{} cycle={}",
                            change_type, direction, w, h, cycle
                        );
                        // Forward direction grows the region; reverse direction shrinks it.
                        if let Some(prev) = previous_region {
                            if direction {
                                assert!(region <= prev);
                            } else {
                                assert!(region >= prev);
                            }
                        }
                        previous_region = Some(region);
                    }
                }
            }
        }
    }

    /// All static map types must partition every tested geometry completely.
    #[test]
    fn static_maps_partition_every_geometry() {
        let geometries = [(2u32, 2u32), (3, 5), (8, 8), (16, 9)];
        for &(w, h) in &geometries {
            let s = sps(w, h);
            let pic_size = w * h;

            let dispersed = SliceGroup::Dispersed {
                num_slice_groups_minus1: 2,
            };
            assert_partition(
                &SliceGroupMap::for_picture(&s, Some(&dispersed), None).unwrap(),
                3,
                pic_size,
            );

            let ids: Vec<u32> = (0..pic_size).map(|i| i % 3).collect();
            let explicit = SliceGroup::ExplicitAssignment {
                num_slice_groups_minus1: 2,
                slice_group_id: ids,
            };
            assert_partition(
                &SliceGroupMap::for_picture(&s, Some(&explicit), None).unwrap(),
                3,
                pic_size,
            );

            // Two groups whose equal-length runs together tile an even-sized picture.
            if pic_size % 2 == 0 {
                let run = pic_size / 2 - 1;
                let interleaved = SliceGroup::Interleaved {
                    run_length_minus1: vec![run, run],
                };
                assert_partition(
                    &SliceGroupMap::for_picture(&s, Some(&interleaved), None).unwrap(),
                    2,
                    pic_size,
                );
            }
        }
    }
}
