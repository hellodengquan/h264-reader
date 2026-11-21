h264-reader
===========

Reader for H264 bitstream syntax, written in Rust.

[![crates.io version](https://img.shields.io/crates/v/h264-reader.svg)](https://crates.io/crates/h264-reader)
[![Documentation](https://docs.rs/h264-reader/badge.svg)](https://docs.rs/h264-reader)

Aims to provide access to stream metadata; does not actually decode the video.

The implementation attempts to minimise copying of source data, for efficiency, at the cost of a more complicated API

## Supported syntax

The following list shows the current state of support per H264 syntax element:

 * Bytestream formats
   * [x] _Annex B_ format (e.g. in MPEG-TS)
   * [x] _AVCC_ format (e.g. in MP4)
 * Network Abstraction Layer Units (NAL Units)
   * [x] `slice_layer_without_partitioning_rbsp()` — `slice_header()` and `slice_data()` (baseline profile only)
   * [ ] `slice_data_partition_a_layer_rbsp()`
   * [ ] `slice_data_partition_b_layer_rbsp()`
   * [ ] `slice_data_partition_c_layer_rbsp()`
   * [ ] `sei_rbsp()` _Supplementary Enhancement Information_ headers - the following payloads are supported:
     * [x] `buffering_period()`
     * [x] `pic_timing()`
     * [ ] `pan_scan_rect()`
     * [ ] `filler_payload()`
     * [x] `user_data_registered_itu_t_t35()`
     * [x] `user_data_unregistered()`
     * [x] `recovery_point()`
     * [ ] `dec_ref_pic_marking_repetition()`
     * [ ] `spare_pic()`
     * [ ] `scene_info()`
     * [ ] `sub_seq_info()`
     * [ ] `sub_seq_layer_characteristics()`
     * [ ] `sub_seq_characteristics()`
     * [ ] `full_frame_freeze()`
     * [ ] `full_frame_freeze_release()`
     * [ ] `full_frame_snapshot()`
     * [ ] `progressive_refinement_segment_start()`
     * [ ] `progressive_refinement_segment_end()`
     * [ ] `motion_constrained_slice_group_set()`
     * [ ] `film_grain_characteristics()`
     * [ ] `deblocking_filter_display_preference()`
     * [ ] `stereo_video_info()`
     * [ ] `post_filter_hint()`
     * [ ] `tone_mapping_info()`
     * [ ] _Annex G_ headers
     * [ ] _Annex H_ headers
     * [ ] _Annex I_ headers
     * [ ] _Annex J_ headers
     * [ ] `frame_packing_arrangement()`
     * [ ] `display_orientation()`
     * [ ] `mastering_display_colour_volume()`
     * [ ] `colour_remapping_info()`
     * [ ] `alternative_transfer_characteristics()`
     * [ ] `alternative_depth_info()`
   * [x] `seq_parameter_set_rbsp()`
   * [x] `pic_parameter_set_rbsp()`
   * [x] `access_unit_delimiter_rbsp()`
   * [ ] `end_of_stream_rbsp()`
   * [ ] `filler_data_rbsp()`
   * [x] `seq_parameter_set_extension_rbsp()`
   * [x] `prefix_nal_unit_rbsp()` — MVC (empty body) and SVC (`prefix_nal_unit_svc()`)
   * [x] `subset_seq_parameter_set_rbsp()`
   * [ ] `depth_parameter_set_rbsp()`
   * [x] `slice_layer_extension_rbsp()` — MVC `slice_header()` only (including `ref_pic_list_mvc_modification()`), not `slice_data()`

## Access unit grouping

Muxing an H.264 elementary stream into MP4 samples, or timing/dropping whole images, needs NAL
units grouped into _access units_ — all the slices of one picture together with the parameter
sets, SEI and other non-VCL NAL units that the standard requires to appear with it.

`h264_reader::push::access_unit::AccessUnitGrouper` sits on top of the normal push parser and
delivers whole access units through the `AccessUnitHandler` trait.  It applies the "first VCL
NAL unit of a primary coded picture" rules of H.264 section 7.4.1.2.3, holds back non-VCL NAL
units that precede a picture until the picture they belong to is known, and only ever examines
complete NAL units — so the grouping is identical regardless of how the input bytes are chunked.
NAL units that cannot be parsed are reported via `AccessUnitHandler::unparseable` without
disturbing the grouping of the NAL units that follow.

```rust
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::push::access_unit::{AccessUnitGrouper, AccessUnitHandler};

#[derive(Default)]
struct Counter { samples: Vec<Vec<UnitType>> }
impl AccessUnitHandler for Counter {
    fn start(&mut self) { self.samples.push(Vec::new()); }
    fn nal(&mut self, nal: RefNal<'_>) {
        self.samples.last_mut().unwrap().push(nal.header().unwrap().nal_unit_type());
    }
}

let mut reader = AnnexBReader::accumulate(AccessUnitGrouper::new(Counter::default()));
// ... call reader.push(bytes) as data arrives ...
reader.reset();                 // end the current Annex B NAL unit
reader.nal_handler_mut().reset(); // flush the final access unit
```

The existing per-NAL push interface (`NalAccumulator` / `AccumulatedNalHandler`) is unchanged.
See the `access_units` example for building length-prefixed MP4 sample bytes.

## Design goals

### Avoid copies

Parsing components accept partial data to avoid coping data
into intermediate buffers.  This is intended to support common cases like,

 - data embedded in MPEG-TS packets, where h264 data is interspersed with MPEG-TS header data
 - data being read from the network, where the data available at any instant may be incomplete

An alternative to accepting partial data would be to take a number of peices of partial data

### Lazy parsing

The implementation should be written to defer parsing data structures until an accessor method is called.
This can mean saving parsing costs for callers that don't care about all the data.  It can be difficult to
apply this principal universally, so in some areas we don't bother and just 'eager parse' (particularly
structures defined bit-by-bit rather than byte-by-byte).
