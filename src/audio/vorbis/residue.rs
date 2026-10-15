//! Vorbis residue decoding (the fine spectral detail multiplied by the floor curve).

use super::bits::BitReader;
use super::codebook::Codebook;

/// A Vorbis residue configuration (types 0, 1 and 2; type 2 is identical to type 1 for the
/// mono streams used here).
pub struct Residue {
    /// Residue type: 0 interleaves vector components across a partition, 1/2 store them in order.
    residue_type: u32,
    /// First spectral coefficient covered by the residue.
    begin: usize,
    /// One past the last spectral coefficient covered by the residue.
    end: usize,
    /// Coefficients per partition.
    partition_size: usize,
    /// Number of partition classifications.
    classifications: usize,
    /// Codebook used to decode partition classifications.
    class_book: usize,
    /// Codebook per (classification, pass), or -1 when that pass is skipped.
    books: Vec<i32>,
}

impl Residue {
    /// Decodes a residue configuration from the setup header.
    pub fn decode(bits: &mut BitReader) -> Self {
        let residue_type = bits.read_bits(16);
        let begin = bits.read_bits(24) as usize;
        let end = bits.read_bits(24) as usize;
        let partition_size = bits.read_bits(24) as usize + 1;
        let classifications = bits.read_bits(6) as usize + 1;
        let class_book = bits.read_bits(8) as usize;

        let cascades: Vec<u32> = (0..classifications)
            .map(|_| {
                let low_bits = bits.read_bits(3);
                let high_bits = if bits.read_flag() {
                    bits.read_bits(5)
                } else {
                    0
                };
                (high_bits << 3) | low_bits
            })
            .collect();
        let books = (0..classifications * 8)
            .map(|i| {
                if cascades[i >> 3] & (1 << (i & 7)) == 0 {
                    -1
                } else {
                    bits.read_bits(8) as i32
                }
            })
            .collect();

        Residue {
            residue_type,
            begin,
            end,
            partition_size,
            classifications,
            class_book,
            books,
        }
    }

    /// Clears `vector[..n]` and, unless `silent`, decodes the residue into it.
    pub fn synthesize(
        &self,
        bits: &mut BitReader,
        codebooks: &[Codebook],
        vector: &mut [f32],
        n: usize,
        silent: bool,
    ) {
        vector[..n].fill(0.0);
        if silent {
            return;
        }

        let class_dimensions = codebooks[self.class_book].dimensions;
        let partitions = (self.end - self.begin) / self.partition_size;
        let mut classes = vec![0usize; partitions];
        for pass in 0..8 {
            let mut partition = 0;
            while partition < partitions {
                if pass == 0 {
                    // One class codeword encodes the classes of several partitions.
                    let mut value = codebooks[self.class_book].decode_scalar(bits);
                    for j in (0..class_dimensions).rev() {
                        if partition + j < partitions {
                            classes[partition + j] = value % self.classifications;
                        }
                        value /= self.classifications;
                    }
                }
                for _ in 0..class_dimensions {
                    let book = self.books[classes[partition] * 8 + pass];
                    if book >= 0 {
                        let start = self.begin + partition * self.partition_size;
                        let codebook = &codebooks[book as usize];
                        if self.residue_type == 0 {
                            let step = self.partition_size / codebook.dimensions;
                            for k in 0..step {
                                let values = codebook.decode_vector(bits);
                                for (d, value) in values.iter().enumerate() {
                                    vector[start + k + d * step] += value;
                                }
                            }
                        } else {
                            let mut k = 0;
                            while k < self.partition_size {
                                let values = codebook.decode_vector(bits);
                                for value in values {
                                    vector[start + k] += value;
                                    k += 1;
                                }
                            }
                        }
                    }
                    partition += 1;
                    if partition >= partitions {
                        break;
                    }
                }
            }
        }
    }
}
