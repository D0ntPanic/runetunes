//! Decoder for the Vorbis-compressed instrument samples in cache archive 14.
//!
//! Jagex stores music samples as raw Vorbis audio packets with the Ogg container and most of the
//! Vorbis headers stripped. All samples share a single setup header (archive 14, group 0, file
//! 0) holding the codebooks, floors, residues, mappings and modes; each sample file then only
//! contains its sample rate, loop points and the audio packets. Streams are always mono, use
//! floor type 1 and do no channel coupling.

mod bits;
mod codebook;
mod floor;
mod residue;

use super::sample::Sample;
use super::Engine;
use crate::buffer::Buffer;
use anyhow::{anyhow, Result};
use bits::{bit_length, BitReader};
use codebook::Codebook;
use floor::Floor;
use residue::Residue;
use std::f64::consts::PI;

/// Precomputed tables for the inverse MDCT of one block size.
struct ImdctTables {
    /// Twiddle factors for the butterfly stages.
    a: Vec<f32>,
    /// Twiddle factors for the final rotation.
    b: Vec<f32>,
    /// Twiddle factors for the pre-rotation step.
    c: Vec<f32>,
    /// Bit-reversal permutation.
    bit_reverse: Vec<usize>,
}

impl ImdctTables {
    fn new(n: usize) -> Self {
        let n2 = n >> 1;
        let n4 = n >> 2;
        let n8 = n >> 3;
        let nf = n as f64;

        let mut a = vec![0.0f32; n2];
        for k in 0..n4 {
            let angle = (k * 4) as f64 * PI / nf;
            a[k * 2] = angle.cos() as f32;
            a[k * 2 + 1] = -(angle.sin() as f32);
        }
        let mut b = vec![0.0f32; n2];
        for k in 0..n4 {
            let angle = (k * 2 + 1) as f64 * PI / (n * 2) as f64;
            b[k * 2] = angle.cos() as f32;
            b[k * 2 + 1] = angle.sin() as f32;
        }
        let mut c = vec![0.0f32; n4];
        for k in 0..n8 {
            let angle = (k * 4 + 2) as f64 * PI / nf;
            c[k * 2] = angle.cos() as f32;
            c[k * 2 + 1] = -(angle.sin() as f32);
        }
        let bits = bit_length(n8 as i32 - 1);
        let bit_reverse = (0..n8)
            .map(|i| {
                let mut value = i as u32;
                let mut result = 0u32;
                for _ in 0..bits {
                    result = (result << 1) | (value & 1);
                    value >>= 1;
                }
                result as usize
            })
            .collect();

        ImdctTables {
            a,
            b,
            c,
            bit_reverse,
        }
    }
}

/// Channel mapping configuration. Mono streams only use the submap selected by `mux`.
struct Mapping {
    /// Submap used by the (single) channel.
    mux: usize,
    /// Floor used by each submap.
    submap_floors: Vec<usize>,
    /// Residue used by each submap.
    submap_residues: Vec<usize>,
}

impl Mapping {
    fn decode(bits: &mut BitReader) -> Self {
        bits.read_bits(16); // Mapping type
        let submaps = if bits.read_flag() {
            bits.read_bits(4) as usize + 1
        } else {
            1
        };
        if bits.read_flag() {
            bits.read_bits(8); // Coupling steps (unused for mono)
        }
        bits.read_bits(2); // Reserved
        let mux = if submaps > 1 {
            bits.read_bits(4) as usize
        } else {
            0
        };
        let mut submap_floors = Vec::with_capacity(submaps);
        let mut submap_residues = Vec::with_capacity(submaps);
        for _ in 0..submaps {
            bits.read_bits(8); // Time configuration (unused)
            submap_floors.push(bits.read_bits(8) as usize);
            submap_residues.push(bits.read_bits(8) as usize);
        }
        Mapping {
            mux,
            submap_floors,
            submap_residues,
        }
    }
}

/// Packet mode: block size and mapping.
struct Mode {
    /// Whether the mode uses the long block size.
    long_block: bool,
    /// Mapping used by the mode.
    mapping: usize,
}

/// The shared Vorbis setup header used by every sample in archive 14.
pub struct VorbisSetup {
    block_size_short: usize,
    block_size_long: usize,
    imdct_short: ImdctTables,
    imdct_long: ImdctTables,
    codebooks: Vec<Codebook>,
    floors: Vec<Floor>,
    residues: Vec<Residue>,
    mappings: Vec<Mapping>,
    modes: Vec<Mode>,
}

impl VorbisSetup {
    /// Decodes the setup header (archive 14, group 0, file 0).
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut bits = BitReader::new(data);
        let block_size_short = 1usize << bits.read_bits(4);
        let block_size_long = 1usize << bits.read_bits(4);

        let codebook_count = bits.read_bits(8) as usize + 1;
        let codebooks = (0..codebook_count)
            .map(|_| Codebook::decode(&mut bits))
            .collect();

        // Time domain transforms are placeholders in Vorbis I.
        let time_count = bits.read_bits(6) + 1;
        for _ in 0..time_count {
            bits.read_bits(16);
        }

        let floor_count = bits.read_bits(6) as usize + 1;
        let floors = (0..floor_count)
            .map(|_| Floor::decode(&mut bits))
            .collect::<Result<_>>()?;
        let residue_count = bits.read_bits(6) as usize + 1;
        let residues = (0..residue_count)
            .map(|_| Residue::decode(&mut bits))
            .collect();
        let mapping_count = bits.read_bits(6) as usize + 1;
        let mappings = (0..mapping_count)
            .map(|_| Mapping::decode(&mut bits))
            .collect();
        let mode_count = bits.read_bits(6) as usize + 1;
        let modes = (0..mode_count)
            .map(|_| {
                let long_block = bits.read_flag();
                bits.read_bits(16); // Window type
                bits.read_bits(16); // Transform type
                let mapping = bits.read_bits(8) as usize;
                Mode {
                    long_block,
                    mapping,
                }
            })
            .collect();

        Ok(VorbisSetup {
            block_size_short,
            block_size_long,
            imdct_short: ImdctTables::new(block_size_short),
            imdct_long: ImdctTables::new(block_size_long),
            codebooks,
            floors,
            residues,
            mappings,
            modes,
        })
    }
}

/// A Vorbis-compressed instrument sample.
pub struct VorbisSample {
    sample_rate: i32,
    /// Total number of decoded samples.
    sample_count: usize,
    loop_start: i32,
    loop_end: i32,
    ping_pong: bool,
    /// Raw Vorbis audio packets.
    packets: Vec<Vec<u8>>,
}

/// State carried from one packet to the next for overlap-add.
struct DecoderState {
    /// Block size of the previous packet (0 before the first packet).
    previous_block_size: usize,
    /// Windowed output of the previous packet.
    previous_vector: Vec<f32>,
    /// Number of samples from the previous packet's right half that overlap with this packet.
    previous_overlap: usize,
    /// Whether the previous packet was silent.
    previous_silent: bool,
    /// Scratch buffer for the current packet. Swapped with `previous_vector` after each packet.
    vector: Vec<f32>,
}

impl VorbisSample {
    /// Parses a sample file from archive 14.
    pub fn decode(data: Vec<u8>) -> Result<Self> {
        let mut data = Buffer::new(data);
        let sample_rate = data.get_i32()?;
        let sample_count = data.get_i32()?;
        let loop_start = data.get_i32()?;
        let mut loop_end = data.get_i32()?;
        let mut ping_pong = false;
        if loop_end < 0 {
            // A negative (bitwise inverted) loop end marks ping-pong looping.
            loop_end = !loop_end;
            ping_pong = true;
        }
        if sample_count < 0 {
            return Err(anyhow!("Invalid Vorbis sample length"));
        }

        let packet_count = data.get_i32()?;
        let mut packets = Vec::with_capacity(packet_count.max(0) as usize);
        for _ in 0..packet_count {
            // Packet length is a sum of bytes, continuing while each byte is 255 (as in Ogg).
            let mut length = 0usize;
            loop {
                let byte = data.get_u8()?;
                length += byte as usize;
                if byte < 255 {
                    break;
                }
            }
            packets.push(data.get_bytes(length)?.to_vec());
        }

        Ok(VorbisSample {
            sample_rate,
            sample_count: sample_count as usize,
            loop_start,
            loop_end,
            ping_pong,
            packets,
        })
    }

    /// Decodes all packets into a [`Sample`], at 8-bit (build 530) or 16-bit (Old School)
    /// resolution.
    pub fn to_sample(&self, setup: &VorbisSetup, engine: Engine) -> Sample {
        let sixteen_bit = engine.sixteen_bit_samples();
        let mut output = vec![0i16; self.sample_count];
        let mut written = 0;
        let mut state = DecoderState {
            previous_block_size: 0,
            previous_vector: vec![0.0; setup.block_size_long],
            previous_overlap: 0,
            previous_silent: false,
            vector: vec![0.0; setup.block_size_long],
        };

        for packet in &self.packets {
            if let Some(decoded) = decode_packet(setup, &mut state, packet, engine) {
                let count = decoded.len().min(self.sample_count - written);
                for &value in &decoded[..count] {
                    output[written] = if sixteen_bit {
                        ((value * 32768.0f32) as i32).clamp(-32768, 32767) as i16
                    } else {
                        let mut scaled = (value * 128.0f32 + 128.0f32) as i32;
                        if scaled as u32 & 0xffff_ff00 != 0 {
                            // Saturate: below range maps to 0, above range to 255 (via -1).
                            scaled = !scaled >> 31;
                        }
                        scaled.wrapping_sub(128) as i8 as i16
                    };
                    written += 1;
                }
            }
        }

        Sample {
            sample_rate: self.sample_rate,
            data: output,
            loop_start: self.loop_start,
            loop_end: self.loop_end,
            ping_pong: self.ping_pong,
        }
    }
}

/// Decodes one audio packet, returning the samples completed by overlapping it with the previous
/// packet (`None` for the first packet).
fn decode_packet(
    setup: &VorbisSetup,
    state: &mut DecoderState,
    packet: &[u8],
    engine: Engine,
) -> Option<Vec<f32>> {
    let mut bits = BitReader::new(packet);
    bits.read_bit(); // Packet type (always audio)
    let mode_number = bits.read_bits(bit_length(setup.modes.len() as i32 - 1)) as usize;
    let mode = &setup.modes[mode_number];
    let long_block = mode.long_block;
    let n = if long_block {
        setup.block_size_long
    } else {
        setup.block_size_short
    };
    let mut previous_long = false;
    let mut next_long = false;
    if long_block {
        previous_long = bits.read_flag();
        next_long = bits.read_flag();
    }

    // Window shapes: a long block next to a short block uses a short slope centred on the
    // block quarter.
    let short = setup.block_size_short;
    let (left_window_start, left_window_end, left_n) = if long_block && !previous_long {
        ((n >> 2) - (short >> 2), (n >> 2) + (short >> 2), short >> 1)
    } else {
        (0, n >> 1, n >> 1)
    };
    let (right_window_start, right_window_end, right_n) = if long_block && !next_long {
        (
            n - (n >> 2) - (short >> 2),
            n + (short >> 2) - (n >> 2),
            short >> 1,
        )
    } else {
        (n >> 1, n, n >> 1)
    };

    let mapping = &setup.mappings[mode.mapping];
    let floor = &setup.floors[mapping.submap_floors[mapping.mux]];
    let mut floor_points = floor.decode_packet(&mut bits, &setup.codebooks);
    let silent = floor_points.is_none();

    let vector = &mut state.vector;
    for &residue in &mapping.submap_residues {
        // Each submap's residue clears the vector first, so only the last one survives (the
        // streams only ever have one submap).
        setup.residues[residue].synthesize(&mut bits, &setup.codebooks, vector, n >> 1, silent);
    }

    if let Some(points) = &mut floor_points {
        floor.synthesize(points, vector, n >> 1);
    }

    if silent {
        vector[n >> 1..n].fill(0.0);
    } else {
        let tables = if long_block {
            &setup.imdct_long
        } else {
            &setup.imdct_short
        };
        inverse_mdct(vector, n, tables, engine);
        for l in left_window_start..left_window_end {
            let x = ((l - left_window_start) as f64 + 0.5) / left_n as f64 * 0.5 * PI;
            let s = x.sin() as f32;
            vector[l] *= (s as f64 * std::f64::consts::FRAC_PI_2 * s as f64).sin() as f32;
        }
        for l in right_window_start..right_window_end {
            let x = ((l - right_window_start) as f64 + 0.5) / right_n as f64 * 0.5 * PI
                + std::f64::consts::FRAC_PI_2;
            let s = x.sin() as f32;
            vector[l] *= (s as f64 * std::f64::consts::FRAC_PI_2 * s as f64).sin() as f32;
        }
    }

    // Overlap-add the right half of the previous block with the left half of this one.
    let mut output = None;
    if state.previous_block_size > 0 {
        let length = (state.previous_block_size + n) >> 2;
        let mut out = vec![0.0f32; length];
        if !state.previous_silent {
            for i in 0..state.previous_overlap {
                out[i] += state.previous_vector[(state.previous_block_size >> 1) + i];
            }
        }
        if !silent {
            for i in left_window_start..n >> 1 {
                out[length + i - (n >> 1)] += vector[i];
            }
        }
        output = Some(out);
    }

    std::mem::swap(&mut state.previous_vector, &mut state.vector);
    state.previous_block_size = n;
    state.previous_overlap = right_window_end - (n >> 1);
    state.previous_silent = silent;
    output
}

/// In-place inverse MDCT of the spectrum in `v[..n/2]`, producing `n` time-domain samples.
fn inverse_mdct(v: &mut [f32], n: usize, tables: &ImdctTables, engine: Engine) {
    let n2 = n >> 1;
    let n4 = n >> 2;
    let n8 = n >> 3;
    let trig_a = &tables.a;
    let trig_b = &tables.b;
    let trig_c = &tables.c;

    for value in v.iter_mut().take(n2) {
        *value *= 0.5f32;
    }
    for k in n2..n {
        v[k] = -v[n - k - 1];
    }

    for k in 0..n4 {
        let a = v[k * 4] - v[n - k * 4 - 1];
        let b = v[k * 4 + 2] - v[n - k * 4 - 3];
        let c = trig_a[k * 2];
        let d = trig_a[k * 2 + 1];
        v[n - k * 4 - 1] = a * c - b * d;
        v[n - k * 4 - 3] = a * d + b * c;
    }

    for k in 0..n8 {
        let a = v[n2 + k * 4 + 3];
        let b = v[n2 + k * 4 + 1];
        let c = v[k * 4 + 3];
        let d = v[k * 4 + 1];
        v[n2 + k * 4 + 3] = a + c;
        v[n2 + k * 4 + 1] = b + d;
        let e = trig_a[n2 - k * 4 - 4];
        let f = trig_a[n2 - k * 4 - 3];
        v[k * 4 + 3] = (a - c) * e - (b - d) * f;
        v[k * 4 + 1] = (b - d) * e + (a - c) * f;
    }

    let log_n = bit_length(n as i32 - 1) as usize;
    for l in 0..log_n - 3 {
        let k0 = n >> (l + 2);
        let k1 = 8 << l;
        for s in 0..2usize << l {
            let i0 = n - k0 * 2 * s;
            let i1 = n - k0 * (s * 2 + 1);
            for r in 0..n >> (l + 4) {
                let r4 = r * 4;
                let a = v[i0 - r4 - 1];
                let b = v[i0 - r4 - 3];
                let c = v[i1 - r4 - 1];
                let d = v[i1 - r4 - 3];
                v[i0 - r4 - 1] = a + c;
                v[i0 - r4 - 3] = b + d;
                let e = trig_a[r * k1];
                let f = trig_a[r * k1 + 1];
                v[i1 - r4 - 1] = (a - c) * e - (b - d) * f;
                v[i1 - r4 - 3] = (b - d) * e + (a - c) * f;
            }
        }
    }

    for l in 1..n8 - 1 {
        let k0 = tables.bit_reverse[l];
        if l < k0 {
            let i = l * 8;
            let j = k0 * 8;
            v.swap(i + 1, j + 1);
            v.swap(i + 3, j + 3);
            v.swap(i + 5, j + 5);
            v.swap(i + 7, j + 7);
        }
    }

    for l in 0..n2 {
        v[l] = v[l * 2 + 1];
    }

    for l in 0..n8 {
        v[n - l * 2 - 1] = v[l * 4];
        v[n - l * 2 - 2] = v[l * 4 + 1];
        v[n - n4 - l * 2 - 1] = v[l * 4 + 2];
        v[n - n4 - l * 2 - 2] = v[l * 4 + 3];
    }

    for l in 0..n8 {
        let b = trig_c[l * 2];
        let c = trig_c[l * 2 + 1];
        let d = v[n2 + l * 2];
        let e = v[n2 + l * 2 + 1];
        let f = v[n - l * 2 - 2];
        let g = v[n - l * 2 - 1];
        let h = c * (d - f) + b * (e + g);
        v[n2 + l * 2] = (d + f + h) * 0.5f32;
        v[n - l * 2 - 2] = (d + f - h) * 0.5f32;
        let i = c * (e + g) - b * (d - f);
        // Float addition isn't associative, and the two clients add these terms in different
        // orders.
        if engine == Engine::Build530 {
            v[n2 + l * 2 + 1] = (e + i - g) * 0.5f32;
            v[n - l * 2 - 1] = (g + i - e) * 0.5f32;
        } else {
            v[n2 + l * 2 + 1] = (e - g + i) * 0.5f32;
            v[n - l * 2 - 1] = (-e + g + i) * 0.5f32;
        }
    }

    for l in 0..n4 {
        v[l] = v[l * 2 + n2] * trig_b[l * 2] + v[l * 2 + n2 + 1] * trig_b[l * 2 + 1];
        v[n2 - l - 1] = v[l * 2 + n2] * trig_b[l * 2 + 1] - v[l * 2 + n2 + 1] * trig_b[l * 2];
    }
    for l in 0..n4 {
        v[n + l - n4] = -v[l];
    }
    for l in 0..n4 {
        v[l] = v[n4 + l];
    }
    for l in 0..n4 {
        v[n4 + l] = -v[n4 - l - 1];
    }
    for l in 0..n4 {
        v[n2 + l] = v[n - l - 1];
    }
}
