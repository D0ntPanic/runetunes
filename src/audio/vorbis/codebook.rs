//! Vorbis codebooks (Huffman trees with optional vector-quantization lookup tables).

use super::bits::{bit_length, BitReader};

/// A Vorbis codebook: a Huffman code over `entry_count` entries, optionally mapping each entry
/// to a `dimensions`-long vector of floats.
pub struct Codebook {
    /// Number of scalars in each value vector.
    pub dimensions: usize,
    /// Decoding tree. Non-negative values are the index to jump to on a 1 bit (a 0 bit moves to
    /// the next index); negative values are leaves holding `!entry`.
    tree: Vec<i32>,
    /// Value vectors for each entry when the codebook has a lookup table.
    value_vectors: Option<Vec<Vec<f32>>>,
}

impl Codebook {
    /// Decodes a codebook from the setup header.
    pub fn decode(bits: &mut BitReader) -> Self {
        bits.read_bits(24); // Sync pattern
        let dimensions = bits.read_bits(16) as usize;
        let entry_count = bits.read_bits(24) as usize;

        let mut lengths = vec![0u32; entry_count];
        if bits.read_flag() {
            // Ordered: runs of entries with increasing code lengths.
            let mut entry = 0;
            let mut length = bits.read_bits(5) + 1;
            while entry < entry_count {
                let run = bits.read_bits(bit_length((entry_count - entry) as i32));
                for _ in 0..run {
                    lengths[entry] = length;
                    entry += 1;
                }
                length += 1;
            }
        } else {
            let sparse = bits.read_flag();
            for length in lengths.iter_mut() {
                *length = if sparse && !bits.read_flag() {
                    0
                } else {
                    bits.read_bits(5) + 1
                };
            }
        }
        let tree = build_tree(&lengths);

        let lookup_type = bits.read_bits(4);
        let value_vectors = if lookup_type > 0 {
            let minimum = float32_unpack(bits.read_bits(32));
            let delta = float32_unpack(bits.read_bits(32));
            let value_bits = bits.read_bits(4) + 1;
            let sequence_p = bits.read_flag();
            let multiplicand_count = if lookup_type == 1 {
                lookup1_values(entry_count as i32, dimensions as i32) as usize
            } else {
                entry_count * dimensions
            };
            let multiplicands: Vec<u32> = (0..multiplicand_count)
                .map(|_| bits.read_bits(value_bits))
                .collect();

            let mut vectors = vec![vec![0.0f32; dimensions]; entry_count];
            for (entry, vector) in vectors.iter_mut().enumerate() {
                let mut last = 0.0f32;
                if lookup_type == 1 {
                    // Lattice: each dimension indexes the multiplicands by a digit of `entry`.
                    let mut divisor = 1usize;
                    for value in vector.iter_mut() {
                        let index = entry / divisor % multiplicand_count;
                        *value = multiplicands[index] as f32 * delta + minimum + last;
                        if sequence_p {
                            last = *value;
                        }
                        divisor = divisor.wrapping_mul(multiplicand_count);
                    }
                } else {
                    let start = entry * dimensions;
                    for (value, &multiplicand) in vector.iter_mut().zip(&multiplicands[start..]) {
                        *value = multiplicand as f32 * delta + minimum + last;
                        if sequence_p {
                            last = *value;
                        }
                    }
                }
            }
            Some(vectors)
        } else {
            None
        };

        Codebook {
            dimensions,
            tree,
            value_vectors,
        }
    }

    /// Decodes one entry number from the bitstream.
    pub fn decode_scalar(&self, bits: &mut BitReader) -> usize {
        let mut i = 0usize;
        while self.tree[i] >= 0 {
            i = if bits.read_bit() == 0 {
                i + 1
            } else {
                self.tree[i] as usize
            };
        }
        !self.tree[i] as usize
    }

    /// Decodes one entry and returns its value vector.
    pub fn decode_vector(&self, bits: &mut BitReader) -> &[f32] {
        let entry = self.decode_scalar(bits);
        &self
            .value_vectors
            .as_ref()
            .expect("Vorbis codebook used for vector decode has no lookup table")[entry]
    }
}

/// Unpacks the Vorbis 32-bit float format (21-bit mantissa, 10-bit exponent, sign).
fn float32_unpack(packed: u32) -> f32 {
    let mut mantissa = (packed & 0x1f_ffff) as i32;
    let exponent = ((packed >> 21) & 0x3ff) as i32;
    if packed & 0x8000_0000 != 0 {
        mantissa = -mantissa;
    }
    (mantissa as f64 * 2.0f64.powf((exponent - 788) as f64)) as f32
}

/// Largest integer `r` such that `r^dimensions <= entries`.
fn lookup1_values(entries: i32, dimensions: i32) -> i32 {
    let mut result = (entries as f64).powf(1.0 / dimensions as f64) as i32 + 1;
    while int_pow(result, dimensions) > entries {
        result -= 1;
    }
    result
}

/// Integer power with Java's wrapping overflow.
fn int_pow(mut base: i32, mut exponent: i32) -> i32 {
    let mut result = 1i32;
    while exponent > 1 {
        if exponent & 1 != 0 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exponent >>= 1;
    }
    if exponent == 1 {
        result.wrapping_mul(base)
    } else {
        result
    }
}

/// Builds the decoding tree from the codeword lengths, assigning canonical Huffman codewords in
/// entry order as the Vorbis specification describes.
fn build_tree(lengths: &[u32]) -> Vec<i32> {
    // Assign codewords, stored left-aligned in a 32-bit word. `next_code[len]` tracks the next
    // free codeword of each length.
    let mut codewords = vec![0u32; lengths.len()];
    let mut next_code = [0u32; 33];
    for (entry, &length) in lengths.iter().enumerate() {
        if length == 0 {
            continue;
        }
        let length = length as usize;
        let bit = 1u32 << (32 - length);
        let code = next_code[length];
        codewords[entry] = code;

        let next;
        if code & bit == 0 {
            next = code | bit;
            // Shorter lengths that shared this prefix must now skip past it.
            let mut shorter = length - 1;
            while shorter >= 1 {
                let shorter_code = next_code[shorter];
                if shorter_code != code {
                    break;
                }
                let shorter_bit = 1u32 << (32 - shorter);
                if shorter_code & shorter_bit != 0 {
                    next_code[shorter] = next_code[shorter - 1];
                    break;
                }
                next_code[shorter] = shorter_code | shorter_bit;
                shorter -= 1;
            }
        } else {
            next = next_code[length - 1];
        }
        next_code[length] = next;
        for longer in length + 1..=32 {
            if next_code[longer] == code {
                next_code[longer] = next;
            }
        }
    }

    // Insert each codeword into a flat binary tree.
    let mut tree = vec![0i32; 8];
    let mut next_free = 0i32;
    for (entry, &length) in lengths.iter().enumerate() {
        if length == 0 {
            continue;
        }
        let code = codewords[entry];
        let mut node = 0i32;
        for bit_index in 0..length {
            let bit = 0x8000_0000u32 >> bit_index;
            if code & bit == 0 {
                node += 1;
            } else {
                if tree[node as usize] == 0 {
                    tree[node as usize] = next_free;
                }
                node = tree[node as usize];
            }
            if node as usize >= tree.len() {
                tree.resize(tree.len() * 2, 0);
            }
        }
        tree[node as usize] = !(entry as i32);
        if node >= next_free {
            next_free = node + 1;
        }
    }
    tree
}
