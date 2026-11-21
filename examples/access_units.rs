//! Groups an Annex B H.264 elementary stream into access units and prints, for
//! each access unit, the NAL units that would make up one MP4 sample (in stream
//! order) and the total sample size in bytes.
//!
//! Usage: `cargo --example access_units path/to/data.h264`

use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::push::access_unit::{AccessUnitGrouper, AccessUnitHandler};
use std::io::Read;

/// Collects the NAL units of each access unit as one MP4 sample.
#[derive(Default)]
struct SampleCollector {
    samples: Vec<Sample>,
    current: Sample,
}

#[derive(Default, Debug)]
struct Sample {
    /// Concatenated NAL units (each prefixed by a 4-byte big-endian length, as
    /// used inside the `mdat` box of an MP4 file).
    data: Vec<u8>,
    nal_unit_types: Vec<UnitType>,
}

impl AccessUnitHandler for SampleCollector {
    fn start(&mut self) {
        self.current = Sample::default();
    }

    fn nal(&mut self, nal: RefNal<'_>) {
        let unit_type = nal.header().unwrap().nal_unit_type();
        let mut bytes = Vec::new();
        nal.reader()
            .read_to_end(&mut bytes)
            .expect("reading complete NAL");
        // AVCC / MP4 length-prefixed framing (4-byte length).
        self.current
            .data
            .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        self.current.data.extend_from_slice(&bytes);
        self.current.nal_unit_types.push(unit_type);
    }

    fn end(&mut self) {
        self.samples.push(std::mem::take(&mut self.current));
    }
}

fn main() {
    let path = {
        let mut args = std::env::args_os();
        if args.len() != 2 {
            eprintln!("Usage: access_units path/to/data.h264");
            std::process::exit(1);
        }
        args.nth(1).unwrap()
    };
    let mut file = std::fs::File::open(path).expect("open");

    // AnnexBReader -> NalAccumulator (inside `accumulate`) -> AccessUnitGrouper.
    let mut reader = AnnexBReader::accumulate(AccessUnitGrouper::new(SampleCollector::default()));
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf).expect("read") {
            0 => break,
            n => reader.push(&buf[..n]),
        }
    }
    // End the current Annex B NAL, then flush the final access unit.
    reader.reset();
    reader.nal_handler_mut().reset();

    let collector = reader.into_nal_handler().into_handler();
    println!("{} access units (MP4 samples):", collector.samples.len());
    for (i, sample) in collector.samples.iter().enumerate() {
        println!(
            "  sample {:4}: {:5} bytes, NAL units {:?}",
            i,
            sample.data.len(),
            sample.nal_unit_types,
        );
    }
}
