//! LSB-first bit reader used by the Vorbis decoder.

/// Reads bits least-significant first, as the Vorbis bitstream specifies.
pub struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
    bit_position: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            position: 0,
            bit_position: 0,
        }
    }

    fn current_byte(&self) -> u32 {
        self.data.get(self.position).copied().unwrap_or(0) as u32
    }

    /// Reads a single bit.
    pub fn read_bit(&mut self) -> u32 {
        let bit = (self.current_byte() >> self.bit_position) & 1;
        self.bit_position += 1;
        self.position += (self.bit_position >> 3) as usize;
        self.bit_position &= 7;
        bit
    }

    /// Reads an unsigned value of `count` bits (0..=32).
    pub fn read_bits(&mut self, mut count: u32) -> u32 {
        let mut result = 0u32;
        let mut shift = 0u32;
        while count >= 8 - self.bit_position {
            let available = 8 - self.bit_position;
            let mask = (1u32 << available) - 1;
            result =
                result.wrapping_add(((self.current_byte() >> self.bit_position) & mask) << shift);
            self.bit_position = 0;
            self.position += 1;
            shift += available;
            count -= available;
        }
        if count > 0 {
            let mask = (1u32 << count) - 1;
            result =
                result.wrapping_add(((self.current_byte() >> self.bit_position) & mask) << shift);
            self.bit_position += count;
        }
        result
    }

    /// Reads a boolean flag bit.
    pub fn read_flag(&mut self) -> bool {
        self.read_bit() != 0
    }
}

/// Number of bits needed to represent `value` (`ilog` in the Vorbis specification).
pub fn bit_length(value: i32) -> u32 {
    32 - (value as u32).leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::{bit_length, BitReader};

    #[test]
    fn bit_length_matches_ilog() {
        assert_eq!(bit_length(0), 0);
        assert_eq!(bit_length(1), 1);
        assert_eq!(bit_length(7), 3);
        assert_eq!(bit_length(8), 4);
        assert_eq!(bit_length(-1), 32);
    }

    #[test]
    fn reads_lsb_first() {
        let mut bits = BitReader::new(&[0b1010_1101, 0xff]);
        assert_eq!(bits.read_bit(), 1);
        assert_eq!(bits.read_bits(3), 0b110);
        assert_eq!(bits.read_bits(8), 0b1111_1010);
        assert_eq!(bits.read_bits(4), 0b1111);
        // Past the end reads as zero.
        assert_eq!(bits.read_bits(8), 0);
    }
}
