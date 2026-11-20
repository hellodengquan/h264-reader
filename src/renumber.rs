//! Re-numbering of H.264 sequence and picture parameter set identifiers.
//!
//! When two independently encoded Annex B streams are concatenated, both halves commonly use
//! the same parameter set id numbers (e.g. both define `SPS 0` / `PPS 0`).  In the joined
//! stream the later definitions override the earlier ones, so every slice in the first half
//! then resolves against the wrong parameter set.
//!
//! This module rewrites a complete bitstream so that parameter sets, and every reference to
//! them, use a caller-supplied numbering.  The affected NAL units are:
//!
//! | NAL unit type                          | Syntax element rewritten         |
//! |----------------------------------------|----------------------------------|
//! | `SeqParameterSet` (7)                  | `seq_parameter_set_id`           |
//! | `SubsetSeqParameterSet` (15, MVC/SVC)  | `seq_parameter_set_id`           |
//! | `SeqParameterSetExtension` (13)        | `seq_parameter_set_id`           |
//! | `PicParameterSet` (8)                  | `pic_parameter_set_id` and the referenced `seq_parameter_set_id` |
//! | slice NAL units (1–5, 19–21)           | `pic_parameter_set_id`           |
//!
//! All other bytes (start codes, SEI, filler data, slice bodies, multiview NAL header
//! extensions, …) are preserved.  NAL units that end up with unchanged identifiers are copied
//! byte for byte, so an identity mapping reproduces the input exactly.
//!
//! The mapping is sparse: any id absent from the mapping keeps its original number.  A mapping
//! is rejected before any output is produced if it cannot be made self-consistent — either an
//! id maps to an invalid value, or two parameter sets of the same kind that coexist in the
//! stream would be assigned the same new id (a collision).

use crate::nal::UnitType;
use crate::rbsp::BitWrite;
use std::convert::TryFrom;
use std::io::Write as _;

/// Number of legal SPS ids (`seq_parameter_set_id` is `ue(v)` constrained to `0..32`).
const SPS_ID_COUNT: usize = 32;
/// Number of legal PPS ids (`pic_parameter_set_id` is `ue(v)` constrained to `0..256`).
const PPS_ID_COUNT: usize = 256;

/// Why a parameter-set re-numbering could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenumberError {
    /// A mapping requested an SPS id outside the legal range (`0..32`).
    InvalidSpsId(u32),
    /// A mapping requested a PPS id outside the legal range (`0..256`).
    InvalidPpsId(u32),
    /// Two sequence parameter sets present in the stream would both receive the same new id.
    /// `(old_id, new_id)` identifies the conflicting target.
    SpsIdCollision { old_id: u8, new_id: u8 },
    /// Two picture parameter sets present in the stream would both receive the same new id.
    /// `(old_id, new_id)` identifies the conflicting target.
    PpsIdCollision { old_id: u8, new_id: u8 },
    /// A NAL unit that must carry a parameter set id was too short or otherwise malformed,
    /// so its id could not be located.
    MalformedNal { nal_unit_type: u8 },
}

impl std::fmt::Display for RenumberError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenumberError::InvalidSpsId(id) => {
                write!(f, "mapped SPS id {id} is outside the legal range 0..32")
            }
            RenumberError::InvalidPpsId(id) => {
                write!(f, "mapped PPS id {id} is outside the legal range 0..256")
            }
            RenumberError::SpsIdCollision { old_id, new_id } => write!(
                f,
                "SPS id {old_id} would collide with another SPS mapped to new id {new_id}"
            ),
            RenumberError::PpsIdCollision { old_id, new_id } => write!(
                f,
                "PPS id {old_id} would collide with another PPS mapped to new id {new_id}"
            ),
            RenumberError::MalformedNal { nal_unit_type } => {
                write!(f, "malformed NAL unit of type {nal_unit_type}")
            }
        }
    }
}

impl std::error::Error for RenumberError {}

/// Caller-supplied mappings for re-numbering the parameter sets in an H.264 bitstream.
///
/// Construct with [`ParamSetRenumberer::new`], then call
/// [`renumber_annexb`](ParamSetRenumberer::renumber_annexb) (or
/// [`renumber_nal`](ParamSetRenumberer::renumber_nal) for a single length-prefixed NAL unit).
///
/// Mappings are sparse: ids that are not mentioned keep their original number.  This makes it
/// safe to pass only the ids of the second, conflicting, fragment when concatenating streams.
#[derive(Debug, Clone)]
pub struct ParamSetRenumberer {
    // `None` means "leave this id unchanged".
    sps: [Option<u8>; SPS_ID_COUNT],
    pps: [Option<u8>; PPS_ID_COUNT],
}

impl Default for ParamSetRenumberer {
    fn default() -> Self {
        Self {
            sps: [None; SPS_ID_COUNT],
            pps: [None; PPS_ID_COUNT],
        }
    }
}

impl ParamSetRenumberer {
    /// Creates a renumberer with an empty (identity) mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a renumberer from explicit mappings, each entry `(old_id, new_id)`.
    ///
    /// Returns an error if any target id is outside the legal range for its kind.
    pub fn with_mappings(
        sps: impl IntoIterator<Item = (u32, u32)>,
        pps: impl IntoIterator<Item = (u32, u32)>,
    ) -> Result<Self, RenumberError> {
        let mut this = Self::new();
        for (old, new) in sps {
            this.map_sps(old, new)?;
        }
        for (old, new) in pps {
            this.map_pps(old, new)?;
        }
        Ok(this)
    }

    /// Records that sequence parameter set `old_id` should be rewritten as `new_id`.
    ///
    /// Both ids must be in the legal SPS range `0..32`.  Multiple calls for the same `old_id`
    /// replace the previous target.
    pub fn map_sps(&mut self, old_id: u32, new_id: u32) -> Result<&mut Self, RenumberError> {
        if old_id >= SPS_ID_COUNT as u32 {
            return Err(RenumberError::InvalidSpsId(old_id));
        }
        if new_id >= SPS_ID_COUNT as u32 {
            return Err(RenumberError::InvalidSpsId(new_id));
        }
        self.sps[old_id as usize] = Some(new_id as u8);
        Ok(self)
    }

    /// Records that picture parameter set `old_id` should be rewritten as `new_id`.
    ///
    /// Both ids must be in the legal PPS range `0..256`.  Multiple calls for the same `old_id`
    /// replace the previous target.
    pub fn map_pps(&mut self, old_id: u32, new_id: u32) -> Result<&mut Self, RenumberError> {
        if old_id >= PPS_ID_COUNT as u32 {
            return Err(RenumberError::InvalidPpsId(old_id));
        }
        if new_id >= PPS_ID_COUNT as u32 {
            return Err(RenumberError::InvalidPpsId(new_id));
        }
        self.pps[old_id as usize] = Some(new_id as u8);
        Ok(self)
    }

    #[inline]
    fn new_sps_id(&self, old: u8) -> u8 {
        self.sps
            .get(usize::from(old))
            .copied()
            .flatten()
            .unwrap_or(old)
    }

    #[inline]
    fn new_pps_id(&self, old: u8) -> u8 {
        self.pps
            .get(usize::from(old))
            .copied()
            .flatten()
            .unwrap_or(old)
    }

    /// Rewrites a whole Annex B byte stream.
    ///
    /// The whole stream is examined before anything is emitted: if the mapping turns out to be
    /// self-inconsistent (two parameter sets of the same kind would share a new id), or a NAL
    /// unit carrying parameter set ids is malformed, the original data is left untouched and the
    /// error is returned.
    ///
    /// NAL units and framing bytes that do not need an edit are copied verbatim, so when every
    /// mapping is identity the returned vector is byte-identical to the input.
    pub fn renumber_annexb(&self, data: &[u8]) -> Result<Vec<u8>, RenumberError> {
        // First pass: validate the mapping against everything the stream contains.
        let mut defined_sps: [bool; SPS_ID_COUNT] = [false; SPS_ID_COUNT];
        let mut defined_pps: [bool; PPS_ID_COUNT] = [false; PPS_ID_COUNT];
        for nal in AnnexBNals::new(data) {
            let nal_type = nal_type(nal.body)?;
            match nal_type {
                // Only SPS and subset SPS actually *define* a parameter set and therefore occupy
                // an id.  An SPS extension (type 13) merely references an existing SPS id, so it
                // is not counted here (it is still rewritten in the second pass to track its
                // referenced SPS).
                UnitType::SeqParameterSet | UnitType::SubsetSeqParameterSet => {
                    let prefix = sps_family_prefix_bits(nal_type);
                    let old = read_first_ue(nal.body, header_len(nal_type), prefix)?;
                    let old = u8::try_from(old).map_err(|_| RenumberError::MalformedNal {
                        nal_unit_type: nal_type.id(),
                    })?;
                    let new = self.new_sps_id(old);
                    if defined_sps[new as usize] {
                        return Err(RenumberError::SpsIdCollision {
                            old_id: old,
                            new_id: new,
                        });
                    }
                    defined_sps[new as usize] = true;
                }
                UnitType::PicParameterSet => {
                    let old_pps = read_first_ue(nal.body, 1, 0)?;
                    let old_pps =
                        u8::try_from(old_pps).map_err(|_| RenumberError::MalformedNal {
                            nal_unit_type: nal_type.id(),
                        })?;
                    let new_pps = self.new_pps_id(old_pps);
                    if defined_pps[new_pps as usize] {
                        return Err(RenumberError::PpsIdCollision {
                            old_id: old_pps,
                            new_id: new_pps,
                        });
                    }
                    defined_pps[new_pps as usize] = true;
                }
                _ => {}
            }
        }

        // Second pass: emit the rewritten stream.  Framing (start codes and any inter-unit
        // bytes) and untouched NAL units are copied verbatim.
        let mut out = Vec::with_capacity(data.len());
        let mut cursor = 0usize;
        for nal in AnnexBNals::new(data) {
            out.extend_from_slice(&data[cursor..nal.start]);
            let rewritten = self.renumber_nal(nal.body)?;
            out.extend_from_slice(&rewritten);
            cursor = nal.end;
        }
        out.extend_from_slice(&data[cursor..]);
        Ok(out)
    }

    /// Rewrites a single NAL unit, supplied without an Annex B start code (the first byte must
    /// be the NAL header).
    ///
    /// This is the primitive used by [`renumber_annexb`](Self::renumber_annexb) and can be used
    /// directly on length-prefixed (AVCC) NAL units.  NAL units that contain no parameter set
    /// ids, or whose ids are all unchanged, are returned as an owned copy of the input.
    pub fn renumber_nal(&self, nal: &[u8]) -> Result<Vec<u8>, RenumberError> {
        let nal_type = nal_type(nal)?;
        let header_len = header_len(nal_type);
        match nal_type {
            UnitType::SeqParameterSet
            | UnitType::SubsetSeqParameterSet
            | UnitType::SeqParameterSetExtension => {
                let prefix = sps_family_prefix_bits(nal_type);
                let old = read_first_ue(nal, header_len, prefix)? as u8;
                let new = self.new_sps_id(old);
                if new == old {
                    return Ok(nal.to_vec());
                }
                replace_ue(nal, header_len, prefix, 0, u32::from(new))
            }
            UnitType::PicParameterSet => {
                // The first ue is pic_parameter_set_id; the second is the referenced sps id.
                let old_pps = read_ue_at(nal, 1, 0, 0)? as u8;
                let old_sps = read_ue_at(nal, 1, 0, 1)? as u8;
                let new_pps = self.new_pps_id(old_pps);
                let new_sps = self.new_sps_id(old_sps);
                if new_pps == old_pps && new_sps == old_sps {
                    return Ok(nal.to_vec());
                }
                let once = replace_ue(nal, 1, 0, 0, u32::from(new_pps))?;
                replace_ue(&once, 1, 0, 1, u32::from(new_sps))
            }
            t if is_slice_type(t) => {
                // Slice headers begin with first_mb_in_slice (kept), then pic_parameter_set_id.
                let old_pps = read_ue_at(nal, header_len, 0, 1)? as u8;
                let new_pps = self.new_pps_id(old_pps);
                if new_pps == old_pps {
                    return Ok(nal.to_vec());
                }
                replace_ue(nal, header_len, 0, 1, u32::from(new_pps))
            }
            _ => Ok(nal.to_vec()),
        }
    }
}

fn is_slice_type(t: UnitType) -> bool {
    matches!(
        t,
        UnitType::SliceLayerWithoutPartitioningNonIdr
            | UnitType::SliceDataPartitionALayer
            | UnitType::SliceDataPartitionBLayer
            | UnitType::SliceDataPartitionCLayer
            | UnitType::SliceLayerWithoutPartitioningIdr
            | UnitType::SliceLayerWithoutPartitioningAux
            | UnitType::SliceExtension
            | UnitType::SliceExtensionViewComponent
    )
}

/// NAL units with a 3-byte MVC/SVC extension use a 4-byte header (1 NAL byte + 3 extension
/// bytes); every other handled NAL unit uses the single-byte header.
fn header_len(t: UnitType) -> usize {
    match t {
        UnitType::SliceExtension | UnitType::SliceExtensionViewComponent => 4,
        _ => 1,
    }
}

/// Number of fixed-width (non-Exp-Golomb) bits at the very start of an SPS-family RBSP, before
/// the first `ue(v)` syntax element.  SPS and subset SPS begin with three bytes
/// (`profile_idc`, `constraint_flags`, `level_idc`); the SPS extension starts directly with
/// `seq_parameter_set_id`.
fn sps_family_prefix_bits(t: UnitType) -> u32 {
    match t {
        UnitType::SeqParameterSet | UnitType::SubsetSeqParameterSet => 24,
        _ => 0,
    }
}

fn nal_type(nal: &[u8]) -> Result<UnitType, RenumberError> {
    let header = *nal
        .first()
        .ok_or(RenumberError::MalformedNal { nal_unit_type: 0 })?;
    // The NAL unit type occupies the low five bits (values 0..31).
    let id = header & 0x1F;
    UnitType::for_id(id).map_err(|_| RenumberError::MalformedNal { nal_unit_type: id })
}

// ------------------------------------------------------------------------------------------------
// Annex B framing
// ------------------------------------------------------------------------------------------------

/// A NAL unit located inside an Annex B buffer: `data[start..end]` is the NAL unit including its
/// header byte but excluding its start code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NalUnit<'a> {
    start: usize,
    end: usize,
    body: &'a [u8],
}

/// Iterator over the NAL units of an Annex B stream, tolerating 3- and 4-byte start codes and
/// leading garbage bytes (which are skipped).
struct AnnexBNals<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBNals<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for AnnexBNals<'a> {
    type Item = NalUnit<'a>;

    fn next(&mut self) -> Option<NalUnit<'a>> {
        // Find the next start-code prefix 0x00 0x00 0x01.
        let mut i = self.pos;
        let body_start;
        loop {
            if i + 2 >= self.data.len() {
                self.pos = self.data.len();
                return None;
            }
            if self.data[i] == 0 && self.data[i + 1] == 0 && self.data[i + 2] == 1 {
                body_start = i + 3;
                break;
            }
            i += 1;
        }
        // Find the next start code `00 00 01` (or end of data).  The NAL unit ends just before
        // it.  When a 4-byte start code (`00 00 00 01`) is used, the leading `zero_byte` is
        // left attached to the preceding NAL unit as a `trailing_zero_8bits`, matching the
        // backtracking behaviour of `AnnexBReader`; it is preserved verbatim (and, for rewritten
        // units, carried through as a post-RBSP byte).
        let mut j = body_start;
        let mut unit_end = self.data.len();
        while j + 2 < self.data.len() {
            if self.data[j] == 0 && self.data[j + 1] == 0 && self.data[j + 2] == 1 {
                unit_end = j;
                break;
            }
            j += 1;
        }
        self.pos = unit_end; // resume scanning at the start code's first zero
        if body_start >= unit_end {
            // Empty unit; skip it and keep scanning.
            return self.next();
        }
        Some(NalUnit {
            start: body_start,
            end: unit_end,
            body: &self.data[body_start..unit_end],
        })
    }
}

// ------------------------------------------------------------------------------------------------
// Emulation-prevention-aware Exp-Golomb editing
// ------------------------------------------------------------------------------------------------

/// Reads the `index`-th Exp-Golomb coded value within the RBSP of `nal` (EBSP form).
///
/// The RBSP begins after `header_len` NAL-header bytes, and the Exp-Golomb sequence starts
/// `prefix_bits` further in (SPS NALs carry three fixed-width bytes before their first `ue`).
fn read_ue_at(
    nal: &[u8],
    header_len: usize,
    prefix_bits: u32,
    index: usize,
) -> Result<u32, RenumberError> {
    let nal_type = nal_type(nal)?;
    let rbsp = strip_emulation_prevention(&nal[header_len..]);
    let mut cur = BitCursor::new(&rbsp);
    cur.skip(prefix_bits).ok_or(RenumberError::MalformedNal {
        nal_unit_type: nal_type.id(),
    })?;
    for _ in 0..=index {
        cur.read_ue().ok_or(RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        })?;
    }
    Ok(cur.last_value())
}

/// Reads the first Exp-Golomb coded value within the RBSP after `prefix_bits` fixed bits.
fn read_first_ue(nal: &[u8], header_len: usize, prefix_bits: u32) -> Result<u32, RenumberError> {
    read_ue_at(nal, header_len, prefix_bits, 0)
}

/// Returns a copy of `nal` with the `index`-th RBSP Exp-Golomb value replaced by `new_value`.
///
/// The edit is performed on the de-escaped RBSP: `prefix_bits` fixed bits precede the
/// Exp-Golomb sequence, the original RBSP trailing bits (stop bit plus alignment padding) are
/// stripped, the payload is re-emitted with the target code word replaced, and fresh trailing
/// bits are appended so the result stays byte aligned.  Any full zero bytes following the RBSP
/// (e.g. `cabac_zero_word` padding) are preserved verbatim.
fn replace_ue(
    nal: &[u8],
    header_len: usize,
    prefix_bits: u32,
    index: usize,
    new_value: u32,
) -> Result<Vec<u8>, RenumberError> {
    let nal_type = nal_type(nal)?;
    let rbsp = strip_emulation_prevention(&nal[header_len..]);
    let mut cur = BitCursor::new(&rbsp);
    cur.skip(prefix_bits).ok_or(RenumberError::MalformedNal {
        nal_unit_type: nal_type.id(),
    })?;

    // Locate the target code word as a span of RBSP bit offsets.
    let mut target_start = 0u32;
    let mut target_len = 0u32;
    for n in 0..=index {
        let bit_start = cur.bit_offset();
        let value = cur.read_ue().ok_or(RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        })?;
        if n == index {
            target_start = bit_start;
            target_len = cur.bit_offset() - bit_start;
            // Re-encoding the original value must reproduce the original code length.
            debug_assert_eq!(ue_len(value), target_len);
        }
    }
    let target_end = target_start + target_len;

    // The RBSP ends with a stop bit (the final `1`) followed by zero alignment bits.  The
    // payload stops just before that final set bit; the trailing bits are regenerated below.
    let stop_bit = rbsp_stop_bit(&rbsp).ok_or(RenumberError::MalformedNal {
        nal_unit_type: nal_type.id(),
    })?;
    if target_end > stop_bit {
        return Err(RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        });
    }

    // Header bytes are carried over untouched (they are not subject to emulation prevention).
    let mut out = Vec::with_capacity(nal.len() + 2);
    out.extend_from_slice(&nal[..header_len]);

    // One shared bit writer over the emulation-prevention byte writer keeps every edit aligned.
    let mut w = crate::rbsp::BitWriter::new(crate::rbsp::ByteWriter::new(&mut out));
    write_bit_range(&mut w, &rbsp, 0, target_start)?;
    w.write_ue(new_value)
        .map_err(|_| RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        })?;
    write_bit_range(&mut w, &rbsp, target_end, stop_bit)?;
    // Fresh stop bit plus zero alignment to the next byte boundary.
    w.write_rbsp_trailing_bits()
        .map_err(|_| RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        })?;
    // Flush the bit writer so the alignment bits reach the byte writer, then preserve any
    // post-RBSP zero padding (cabac_zero_word) that followed the original trailing bits.
    let byte_writer = w.into_writer().into_writer();
    byte_writer
        .write_all(&rbsp[(stop_bit / 8) as usize + 1..])
        .map_err(|_| RenumberError::MalformedNal {
            nal_unit_type: nal_type.id(),
        })?;
    Ok(out)
}

/// Writes the source RBSP bits in `[start_bit, end_bit)` through a bit writer.
///
/// Source bit offsets are independent of the writer's current position (the replacement code
/// word can be a different length than the original), so each bit is read from the source at
/// its own offset and appended to the writer in order.
fn write_bit_range<W: BitWrite>(
    w: &mut W,
    rbsp: &[u8],
    start_bit: u32,
    end_bit: u32,
) -> Result<(), RenumberError> {
    debug_assert!(start_bit <= end_bit);
    debug_assert!(end_bit <= rbsp.len() as u32 * 8);
    for bit in start_bit..end_bit {
        let byte = rbsp[(bit / 8) as usize];
        let set = (byte >> (7 - bit % 8)) & 1 == 1;
        w.write_bit(set)
            .map_err(|_| RenumberError::MalformedNal { nal_unit_type: 0 })?;
    }
    Ok(())
}

/// Returns the bit offset of the RBSP stop bit, which is the last set (`1`) bit in the RBSP:
/// `rbsp_trailing_bits()` is a single one followed by zero alignment bits.  Returns `None` if
/// the RBSP contains no set bit (and therefore no valid stop bit).
fn rbsp_stop_bit(rbsp: &[u8]) -> Option<u32> {
    let mut last_one: Option<u32> = None;
    for (i, &byte) in rbsp.iter().enumerate() {
        if byte != 0 {
            // MSB-first position of the lowest set bit in this byte is the last set bit.
            let within = 7 - byte.trailing_zeros();
            last_one = Some(i as u32 * 8 + within);
        }
    }
    last_one
}

/// Length in bits of the Exp-Golomb code for `value`.
fn ue_len(value: u32) -> u32 {
    if value == 0 {
        1
    } else {
        let code_num = value + 1;
        let m = 32 - code_num.leading_zeros(); // floor(log2(code_num))
        2 * m - 1
    }
}

/// Removes emulation-prevention (`0x00 0x00 0x03`) three-bytes from an EBSP RBSP.
fn strip_emulation_prevention(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut i = 0;
    while i < ebsp.len() {
        if i + 2 < ebsp.len() && ebsp[i] == 0 && ebsp[i + 1] == 0 && ebsp[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(ebsp[i]);
            i += 1;
        }
    }
    out
}

/// Cursor over a de-escaped RBSP byte slice, decoding Exp-Golomb syntax elements while
/// tracking bit positions.
struct BitCursor<'a> {
    rbsp: &'a [u8],
    bit_offset: u32,
    last: u32,
}

impl<'a> BitCursor<'a> {
    fn new(rbsp: &'a [u8]) -> Self {
        Self {
            rbsp,
            bit_offset: 0,
            last: 0,
        }
    }

    fn bit_offset(&self) -> u32 {
        self.bit_offset
    }

    fn last_value(&self) -> u32 {
        self.last
    }

    /// Advances over `bits` fixed-width (non-Exp-Golomb) bits, returning `None` if they run past
    /// the end of the RBSP.
    fn skip(&mut self, bits: u32) -> Option<()> {
        if self.bit_offset + bits > self.rbsp.len() as u32 * 8 {
            return None;
        }
        self.bit_offset += bits;
        Some(())
    }

    fn next_bit(&mut self) -> Option<bool> {
        let byte_index = (self.bit_offset / 8) as usize;
        let within = self.bit_offset % 8;
        let byte = *self.rbsp.get(byte_index)?;
        self.bit_offset += 1;
        Some((byte >> (7 - within)) & 1 == 1)
    }

    /// Decodes one Exp-Golomb value, returning `None` on truncation.
    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        loop {
            if !self.next_bit()? {
                zeros += 1;
                if zeros > 31 {
                    return None;
                }
            } else {
                break;
            }
        }
        let mut suffix = 0u32;
        for _ in 0..zeros {
            suffix = (suffix << 1) | u32::from(self.next_bit()?);
        }
        let value = if zeros == 0 {
            0
        } else {
            (1u32 << zeros) - 1 + suffix
        };
        self.last = value;
        Some(value)
    }
}

// ------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal::pps::PicParameterSet;
    use crate::nal::sps::{SeqParamSetId, SeqParameterSet};
    use crate::nal::sps_extension::SeqParameterSetExtension;
    use crate::nal::{NalHeader, UnitType};
    use crate::rbsp::{decode_nal, BitReader, BitWrite, BitWriter, ByteWriter};
    use crate::Context;

    fn sc4() -> Vec<u8> {
        vec![0, 0, 0, 1]
    }

    /// Builds an EBSP NAL unit from its header and RBSP bytes.
    fn ebsp(header: u8, rbsp: &[u8]) -> Vec<u8> {
        let mut out = vec![header];
        ByteWriter::new(&mut out).write_all(rbsp).unwrap();
        out
    }

    #[test]
    fn identity_mapping_is_byte_identical() {
        // SPS + PPS + IDR slice, the SPS containing emulation-prevention sequences.
        let sps = ebsp(0x67, &sps_rbsp(0));
        let pps = ebsp(0x68, &pps_rbsp(0, 0));
        let slice = ebsp(0x65, &slice_header_rbsp(0, &[0xAB, 0xCD]));
        let mut stream = sc4();
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&slice);

        let out = ParamSetRenumberer::new().renumber_annexb(&stream).unwrap();
        assert_eq!(out, stream, "identity mapping must be byte-identical");
    }

    #[test]
    fn renumbers_sps_pps_and_slice_references() {
        // SPS id 0 -> 3 ; PPS id 0 -> 5 (referenced SPS 0 -> 3) ; slice pps 0 -> 5.
        let sps = ebsp(0x67, &sps_rbsp(0));
        let pps = ebsp(0x68, &pps_rbsp(0, 0));
        let slice = ebsp(0x65, &slice_header_rbsp(0, &[0xAB, 0xCD]));
        let mut stream = sc4();
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&pps);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&slice);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 3).unwrap();
        r.map_pps(0, 5).unwrap();
        let out = r.renumber_annexb(&stream).unwrap();

        let nals = collect_nals(&out);
        assert_eq!(nals.len(), 3);

        let sps_out =
            SeqParameterSet::from_bits(BitReader::new(&decode_nal(&nals[0]).unwrap()[..])).unwrap();
        assert_eq!(sps_out.id().id(), 3);

        // PPS needs the renumbered SPS registered under its new id.
        let mut ctx = Context::new();
        ctx.put_seq_param_set(sps_out);
        let pps_out =
            PicParameterSet::from_bits(&ctx, BitReader::new(&decode_nal(&nals[1]).unwrap()[..]))
                .unwrap();
        assert_eq!(pps_out.pic_parameter_set_id.id(), 5);
        assert_eq!(pps_out.seq_parameter_set_id.id(), 3);

        // Slice: first ue unchanged (first_mb=0), second ue is pps_id=5.
        assert_eq!(read_ue_at(&nals[2], 1, 0, 0).unwrap(), 0);
        assert_eq!(read_ue_at(&nals[2], 1, 0, 1).unwrap(), 5);
        // The untouched payload suffix (0xAB, 0xCD = 16 bits) survives regardless of the changed
        // pps id code length: read past both leading ue values in each RBSP and compare.
        let suffix_bits = |rbsp: &[u8]| -> Vec<u8> {
            let mut cur = BitCursor::new(rbsp);
            cur.read_ue().unwrap(); // first_mb_in_slice
            cur.read_ue().unwrap(); // pic_parameter_set_id
            let start = cur.bit_offset();
            bit_slice(rbsp, start, start + 16)
        };
        let original_rbsp = strip_emulation_prevention(&slice[1..]);
        let rewritten_rbsp = strip_emulation_prevention(&nals[2][1..]);
        assert_eq!(suffix_bits(&rewritten_rbsp), suffix_bits(&original_rbsp));
    }

    #[test]
    fn unchanged_nals_and_three_byte_start_codes_are_copied_verbatim() {
        let aud = vec![0x09, 0x10];
        let sei = vec![0x06, 0x05, 0xFF];
        let mut stream = vec![0, 0, 1]; // 3-byte start code, deliberately not 4
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&sei);

        let out = ParamSetRenumberer::new().renumber_annexb(&stream).unwrap();
        assert_eq!(out, stream);
    }

    #[test]
    fn sps_id_collision_is_rejected() {
        // Two distinct SPS ids (0 and 1) both mapped onto 2.
        let sps0 = ebsp(0x67, &sps_rbsp(0));
        let sps1 = ebsp(0x67, &sps_rbsp(1));
        let mut stream = sc4();
        stream.extend_from_slice(&sps0);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&sps1);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 2).unwrap();
        r.map_sps(1, 2).unwrap();
        assert!(matches!(
            r.renumber_annexb(&stream).unwrap_err(),
            RenumberError::SpsIdCollision {
                old_id: 1,
                new_id: 2
            }
        ));
    }

    #[test]
    fn implicit_collision_with_unmapped_id_is_rejected() {
        // SPS 0 stays 0 (not in the map), SPS 1 explicitly mapped to 0 -> collision.
        let sps0 = ebsp(0x67, &sps_rbsp(0));
        let sps1 = ebsp(0x67, &sps_rbsp(1));
        let mut stream = sc4();
        stream.extend_from_slice(&sps0);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&sps1);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(1, 0).unwrap();
        assert!(matches!(
            r.renumber_annexb(&stream).unwrap_err(),
            RenumberError::SpsIdCollision {
                old_id: 1,
                new_id: 0
            }
        ));
    }

    #[test]
    fn pps_collision_is_rejected() {
        let pps0 = ebsp(0x68, &pps_rbsp(0, 0));
        let pps1 = ebsp(0x68, &pps_rbsp(1, 0));
        let mut stream = sc4();
        stream.extend_from_slice(&pps0);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&pps1);

        let mut r = ParamSetRenumberer::new();
        r.map_pps(1, 0).unwrap();
        assert!(matches!(
            r.renumber_annexb(&stream).unwrap_err(),
            RenumberError::PpsIdCollision {
                old_id: 1,
                new_id: 0
            }
        ));
    }

    #[test]
    fn out_of_range_mapping_targets_are_rejected() {
        let mut r = ParamSetRenumberer::new();
        // Out-of-range target ids.
        assert!(matches!(
            r.map_sps(0, 32).unwrap_err(),
            RenumberError::InvalidSpsId(32)
        ));
        assert!(matches!(
            r.map_pps(0, 256).unwrap_err(),
            RenumberError::InvalidPpsId(256)
        ));
        // Out-of-range source ids (must return an error rather than indexing out of bounds).
        assert!(matches!(r.map_sps(31, 0).unwrap(), _));
        assert!(matches!(
            r.map_sps(32, 0).unwrap_err(),
            RenumberError::InvalidSpsId(32)
        ));
        assert!(matches!(
            r.map_pps(300, 0).unwrap_err(),
            RenumberError::InvalidPpsId(300)
        ));
    }

    #[test]
    fn emulation_prevention_bytes_are_handled() {
        // Build a PPS RBSP whose body contains a `00 00 00` pattern: after EBSP escaping it must
        // carry a `00 00 03 00` sequence, and the rewrite must survive de-escaping/re-escaping.
        let mut rbsp = Vec::new();
        {
            let mut w = BitWriter::new(&mut rbsp);
            w.write_ue(0).unwrap(); // pps id 0
            w.write_ue(0).unwrap(); // sps id 0
                                    // Force several all-zero bytes into the body (a field pattern with many zero bits),
                                    // enough that EBSP escaping must insert an emulation-prevention three byte.
            for _ in 0..32 {
                w.write_bit(false).unwrap();
            }
            w.write_rbsp_trailing_bits().unwrap();
        }
        let nal = ebsp(0x68, &rbsp);
        assert!(
            nal.windows(3).any(|w| w == [0, 0, 3]),
            "test fixture should contain an emulation-prevention sequence; got {:02x?}",
            nal
        );

        let mut r = ParamSetRenumberer::new();
        r.map_pps(0, 2).unwrap();
        let out = r.renumber_nal(&nal).unwrap();
        // The rewritten NAL must itself remain a valid EBSP: de-escaping then re-escaping is
        // stable, and the new id reads back correctly.
        assert_eq!(read_ue_at(&out, 1, 0, 0).unwrap(), 2);
        let re_escaped = {
            let de = strip_emulation_prevention(&out[1..]);
            let mut v = vec![0x68u8];
            ByteWriter::new(&mut v).write_all(&de).unwrap();
            v
        };
        assert_eq!(re_escaped, out);
    }

    #[test]
    fn multiview_subset_sps_and_extended_header_slice() {
        // Subset SPS (type 15) with id 0 -> mapped to 2.  Its body is an SPS-shaped RBSP; the
        // renumberer only touches the first ue after the 24-bit fixed prefix, so we verify the id
        // at the syntax level rather than requiring a full multiview SubsetSps decode.
        let subset = ebsp(0x6F, &sps_rbsp(0));
        // MVC slice NAL (type 20): 4-byte header, then slice header RBSP
        // first_mb=0 (ue '1'), pps_id=0 (ue '1'), payload.
        // 0x74 = nal_ref_idc 3, type 20.  Extension bytes chosen so the header contains no
        // forbidden `00 00 0x` sequence (a real encoder would emulation-prevent such a pattern),
        // keeping the test focused on id rewriting rather than framing escapes.
        let mvc_header = [0x74, 0x10, 0x20, 0x40];
        // Build the whole NAL through one ByteWriter so emulation prevention is applied with the
        // correct leading-byte context (the 4-byte MVC header ends in two 0x00 bytes).
        let mut mvc_slice = Vec::new();
        {
            let mut bw = ByteWriter::new(&mut mvc_slice);
            bw.write_all(&mvc_header).unwrap();
            bw.write_all(&slice_header_rbsp(0, &[0x77])).unwrap();
        }

        let mut stream = sc4();
        stream.extend_from_slice(&subset);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&mvc_slice);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 2).unwrap();
        r.map_pps(0, 9).unwrap();
        let out = r.renumber_annexb(&stream).unwrap();
        let nals = collect_nals(&out);

        // The subset SPS id (first ue after the 24-bit fixed prefix) is now 2.
        assert_eq!(read_first_ue(&nals[0], 1, 24).unwrap(), 2);

        // The full 4-byte MVC header (including its emulation-prevention encoding) is preserved
        // byte for byte: the renumberer treats it as an opaque fixed prefix and only edits the
        // RBSP that follows.
        assert_eq!(&nals[1][..4], &mvc_slice[..4]);
        assert_eq!(nals[1][0] & 0x1F, 20); // still NAL type 20 (5-bit field)
                                           // PPS id is the second ue of the RBSP (after the 4-byte header).
        assert_eq!(read_ue_at(&nals[1], 4, 0, 0).unwrap(), 0);
        assert_eq!(read_ue_at(&nals[1], 4, 0, 1).unwrap(), 9);
        // The opaque 0x77 payload survives regardless of the pps id code's changed length: read
        // past both leading ue values in each RBSP and compare the following 8 bits.
        let suffix_bits = |rbsp: &[u8]| -> Vec<u8> {
            let mut cur = BitCursor::new(rbsp);
            cur.read_ue().unwrap(); // first_mb_in_slice
            cur.read_ue().unwrap(); // pic_parameter_set_id
            let start = cur.bit_offset();
            bit_slice(rbsp, start, start + 8)
        };
        let original_rbsp = strip_emulation_prevention(&mvc_slice[4..]);
        let rewritten_rbsp = strip_emulation_prevention(&nals[1][4..]);
        assert_eq!(suffix_bits(&rewritten_rbsp), suffix_bits(&original_rbsp));
    }

    #[test]
    fn sps_extension_nal_is_renumbered() {
        // Type 13: seq_parameter_set_id is the very first ue (id 0 here).
        let ext = ebsp(0x6D, &[0b1101_0000]);
        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 4).unwrap();
        let out = r.renumber_nal(&ext).unwrap();
        let parsed =
            SeqParameterSetExtension::from_bits(BitReader::new(&decode_nal(&out).unwrap()[..]))
                .unwrap();
        assert_eq!(parsed.seq_parameter_set_id.id(), 4);
    }

    #[test]
    fn sps_and_its_extension_sharing_an_id_do_not_collide() {
        // A type-13 SPS extension *references* an SPS id rather than defining a new parameter
        // set, so an SPS and its extension carrying the same id must not be treated as a
        // collision.  Both are rewritten to the new id consistently.
        let sps = ebsp(0x67, &sps_rbsp(0));
        let ext = ebsp(0x6D, &[0b1101_0000]);
        let mut stream = sc4();
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&sc4());
        stream.extend_from_slice(&ext);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 6).unwrap();
        let out = r.renumber_annexb(&stream).unwrap();
        let nals = collect_nals(&out);

        let sps_out =
            SeqParameterSet::from_bits(BitReader::new(&decode_nal(&nals[0]).unwrap()[..])).unwrap();
        assert_eq!(sps_out.id().id(), 6);
        let ext_out =
            SeqParameterSetExtension::from_bits(BitReader::new(&decode_nal(&nals[1]).unwrap()[..]))
                .unwrap();
        assert_eq!(ext_out.seq_parameter_set_id.id(), 6);
    }

    #[test]
    fn malformed_id_nal_is_rejected() {
        // A PPS header with no RBSP at all.
        let broken = vec![0x68u8];
        let r = ParamSetRenumberer::new();
        assert!(matches!(
            r.renumber_nal(&broken).unwrap_err(),
            RenumberError::MalformedNal { nal_unit_type: 8 }
        ));
    }

    #[test]
    fn unrelated_nal_types_pass_through_unchanged() {
        // AUD (type 9) has no parameter set ids: must come back identical.
        let aud = vec![0x09u8, 0x10, 0x00];
        let r = ParamSetRenumberer::new();
        assert_eq!(r.renumber_nal(&aud).unwrap(), aud);
    }

    #[test]
    fn concatenated_fragments_both_parse_with_renumbered_second_half() {
        // Motivating use case: fragment A and B each define SPS0/PPS0 and a slice using PPS0.
        // Renumber fragment B alone (SPS 0->1, PPS 0->1) then concatenate; the joined stream
        // must parse with both parameter sets coexisting and both slices resolving correctly.
        fn fragment(sps_id: u8, pps_id: u8) -> Vec<u8> {
            let mut s = sc4();
            s.extend_from_slice(&ebsp(0x67, &sps_rbsp(sps_id)));
            s.extend_from_slice(&sc4());
            s.extend_from_slice(&ebsp(0x68, &pps_rbsp(pps_id, sps_id)));
            s.extend_from_slice(&sc4());
            s.extend_from_slice(&ebsp(0x65, &slice_header_rbsp(pps_id, &[0x42])));
            s
        }
        let frag_a = fragment(0, 0);
        let frag_b = fragment(0, 0);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 1).unwrap();
        r.map_pps(0, 1).unwrap();
        let frag_b_renumbered = r.renumber_annexb(&frag_b).unwrap();

        let mut combined = frag_a.clone();
        combined.extend_from_slice(&frag_b_renumbered);

        // Full parse of the combined stream with a fresh context.
        let mut ctx = Context::new();
        let mut seen_sps = Vec::new();
        let mut seen_pps = Vec::new();
        let mut slice_pps = Vec::new();
        for nal in collect_nals(&combined) {
            let header = NalHeader::new(nal[0]).unwrap();
            match header.nal_unit_type() {
                UnitType::SeqParameterSet => {
                    let sps =
                        SeqParameterSet::from_bits(BitReader::new(&decode_nal(&nal).unwrap()[..]))
                            .unwrap();
                    seen_sps.push(sps.id().id());
                    ctx.put_seq_param_set(sps);
                }
                UnitType::PicParameterSet => {
                    let pps = PicParameterSet::from_bits(
                        &ctx,
                        BitReader::new(&decode_nal(&nal).unwrap()[..]),
                    )
                    .unwrap();
                    seen_pps.push(pps.pic_parameter_set_id.id());
                    ctx.put_pic_param_set(pps);
                }
                UnitType::SliceLayerWithoutPartitioningIdr => {
                    slice_pps.push(read_ue_at(&nal, 1, 0, 1).unwrap());
                }
                _ => {}
            }
        }
        assert_eq!(seen_sps, vec![0, 1]);
        assert_eq!(seen_pps, vec![0, 1]);
        assert_eq!(slice_pps, vec![0, 1]);

        // Applying the same conflicting mapping to the whole joined stream in one pass is
        // refused, since it would collapse both SPS0 definitions onto SPS1.
        let mut joined = frag_a;
        joined.extend_from_slice(&frag_b);
        assert!(r.renumber_annexb(&joined).is_err());
    }

    #[test]
    fn ue_code_lengths() {
        assert_eq!(ue_len(0), 1);
        assert_eq!(ue_len(1), 3);
        assert_eq!(ue_len(2), 3);
        assert_eq!(ue_len(3), 5);
        assert_eq!(ue_len(31), 11);
    }

    #[test]
    fn annex_b_scanner_handles_three_and_four_byte_start_codes() {
        let data = [
            0xAA, // leading garbage
            0, 0, 1, 0x09, 0x10, // 3-byte code + AUD (no trailing zero)
            0, 0, 0, 1, 0x06, 0x05, // 4-byte code + SEI; its zero_byte stays on the AUD
        ];
        let units: Vec<_> = AnnexBNals::new(&data).map(|u| u.body.to_vec()).collect();
        assert_eq!(units, vec![vec![0x09, 0x10, 0x00], vec![0x06, 0x05]]);
    }

    #[test]
    fn annex_b_scanner_keeps_trailing_zero_byte_of_four_byte_code() {
        // The SPS body ends in a data byte, immediately followed by a 3-byte start code.
        let data = [
            0, 0, 1, 0x67, 0x42, 0x00, // SPS body
            0, 0, 1, 0x68, 0xCE, // PPS after a 3-byte code
        ];
        let units: Vec<_> = AnnexBNals::new(&data).map(|u| u.body.to_vec()).collect();
        assert_eq!(units, vec![vec![0x67, 0x42, 0x00], vec![0x68, 0xCE]]);

        // With a 4-byte start code, the leading `zero_byte` stays attached to the preceding NAL
        // as a trailing_zero_8bits (matching AnnexBReader's backtracking).
        let data4 = [
            0, 0, 0, 1, 0x09, 0x10, // AUD
            0, 0, 0, 1, 0x06, 0x05, // SEI
        ];
        let units4: Vec<_> = AnnexBNals::new(&data4).map(|u| u.body.to_vec()).collect();
        assert_eq!(units4, vec![vec![0x09, 0x10, 0x00], vec![0x06, 0x05]]);
    }

    #[test]
    fn framing_is_preserved_when_a_middle_nal_is_rewritten() {
        // Mixed 3- and 4-byte start codes and an SPS whose id moves; every framing byte and
        // every untouched NAL must survive verbatim around the rewritten middle NAL.  The PPS
        // references an SPS id (5) that is not remapped, so it is left completely untouched.
        let sps = ebsp(0x67, &sps_rbsp(0));
        let pps = ebsp(0x68, &pps_rbsp(0, 5));
        let aud = vec![0x09, 0x10];
        let code3: Vec<u8> = vec![0, 0, 1];
        let code4: Vec<u8> = vec![0, 0, 0, 1];
        let mut stream = Vec::new();
        stream.extend_from_slice(&code3); // 3-byte
        stream.extend_from_slice(&aud);
        stream.extend_from_slice(&code4); // 4-byte
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&code3); // 3-byte
        stream.extend_from_slice(&pps);

        let mut r = ParamSetRenumberer::new();
        r.map_sps(0, 11).unwrap();
        let out = r.renumber_annexb(&stream).unwrap();

        // Both buffers contain the same number of start codes in the same order; only the bytes
        // of the rewritten middle NAL differ in length.
        let start_code_count = |d: &[u8]| {
            (0..d.len().saturating_sub(2))
                .filter(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)
                .count()
        };
        assert_eq!(start_code_count(&out), start_code_count(&stream));

        let nals = collect_nals(&out);
        // The untouched AUD and PPS bodies (ignoring any trailing zero_byte framing) pass
        // through byte for byte; only the SPS changes.
        assert!(nals[0].starts_with(&aud[..]));
        assert_eq!(nals[2], pps);
        let sps_out =
            SeqParameterSet::from_bits(BitReader::new(&decode_nal(&nals[1]).unwrap()[..])).unwrap();
        assert_eq!(sps_out.id().id(), 11);

        // The exact framing prefix up to the first byte of the SPS is unchanged.
        let sps_prefix = code3.len() + aud.len() + code4.len();
        assert_eq!(&out[..sps_prefix], &stream[..sps_prefix]);
    }

    // ---- fixtures -----------------------------------------------------------------------------

    fn collect_nals(data: &[u8]) -> Vec<Vec<u8>> {
        AnnexBNals::new(data).map(|u| u.body.to_vec()).collect()
    }

    /// Extracts `[start, end)` bits (MSB-first) from an RBSP byte slice, returned as a vector of
    /// bit values (0 or 1).
    fn bit_slice(rbsp: &[u8], start: u32, end: u32) -> Vec<u8> {
        (start..end)
            .map(|bit| (rbsp[(bit / 8) as usize] >> (7 - bit % 8)) & 1)
            .collect()
    }

    /// Parseable SPS RBSP (High profile, 16x16, poc type 2) carrying the given id.
    fn sps_rbsp(id: u8) -> Vec<u8> {
        use crate::nal::sps::{
            ChromaInfo, FrameMbsFlags, PicOrderCntType, ProfileIdc, SeqParameterSet,
        };
        let sps = SeqParameterSet {
            profile_idc: ProfileIdc::from(100u8),
            constraint_flags: 0u8.into(),
            level_idc: 10,
            seq_parameter_set_id: SeqParamSetId::from_u32(u32::from(id)).unwrap(),
            chroma_info: ChromaInfo::default(),
            log2_max_frame_num_minus4: 0,
            pic_order_cnt: PicOrderCntType::TypeTwo,
            max_num_ref_frames: 1,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_flags: FrameMbsFlags::Frames,
            direct_8x8_inference_flag: true,
            frame_cropping: None,
            vui_parameters: None,
        };
        let mut rbsp = Vec::new();
        crate::nal::WritableNal::write_bits(&sps, &mut BitWriter::new(&mut rbsp)).unwrap();
        rbsp
    }

    /// Minimal PPS RBSP with the given pps id and referenced sps id, encoded in exact spec
    /// field order so that `PicParameterSet::from_bits` accepts it.
    fn pps_rbsp(pps_id: u8, sps_id: u8) -> Vec<u8> {
        let mut rbsp = Vec::new();
        {
            let mut w = BitWriter::new(&mut rbsp);
            w.write_ue(u32::from(pps_id)).unwrap();
            w.write_ue(u32::from(sps_id)).unwrap();
            w.write_bit(false).unwrap(); // entropy_coding_mode_flag (CAVLC)
            w.write_bit(false).unwrap(); // bottom_field_pic_order_in_frame_present_flag
            w.write_ue(0).unwrap(); // num_slice_groups_minus1
            w.write_ue(0).unwrap(); // num_ref_idx_l0_default_active_minus1
            w.write_ue(0).unwrap(); // num_ref_idx_l1_default_active_minus1
            w.write_bit(false).unwrap(); // weighted_pred_flag
            w.write::<2, u8>(0).unwrap(); // weighted_bipred_idc
            w.write_se(0).unwrap(); // pic_init_qp_minus26
            w.write_se(0).unwrap(); // pic_init_qs_minus26
            w.write_se(0).unwrap(); // chroma_qp_index_offset
            w.write_bit(true).unwrap(); // deblocking_filter_control_present_flag
            w.write_bit(false).unwrap(); // constrained_intra_pred_flag
            w.write_bit(false).unwrap(); // redundant_pic_cnt_present_flag
            w.write_rbsp_trailing_bits().unwrap();
        }
        rbsp
    }

    /// Slice header RBSP beginning with first_mb_in_slice=0 and pic_parameter_set_id=`pps_id`,
    /// followed by an opaque payload suffix (kept verbatim through rewriting) and a single RBSP
    /// stop-bit terminator.  The suffix bytes are emitted as bits through the same writer so the
    /// result is one coherent, byte-aligned RBSP with no forbidden start-code sequences.
    fn slice_header_rbsp(pps_id: u8, suffix: &[u8]) -> Vec<u8> {
        let mut rbsp = Vec::new();
        {
            let mut w = BitWriter::new(&mut rbsp);
            w.write_ue(0).unwrap(); // first_mb_in_slice
            w.write_ue(u32::from(pps_id)).unwrap();
            for &byte in suffix {
                w.write::<8, u8>(byte).unwrap();
            }
            w.write_rbsp_trailing_bits().unwrap();
        }
        rbsp
    }
}
