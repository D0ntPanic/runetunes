use anyhow::{anyhow, Result};
use std::rc::Rc;

/// Data buffer stream
#[derive(Clone)]
pub struct Buffer {
    data: Rc<Vec<u8>>,
    offset: usize,
}

/// Output data stream
pub struct OutputBuffer {
    data: Vec<u8>,
}

impl Buffer {
    /// Creates a new buffer from the given data
    pub fn new(data: Vec<u8>) -> Self {
        Buffer {
            data: Rc::new(data),
            offset: 0,
        }
    }

    /// Seeks to an absolute offset
    pub fn seek_absolute(&mut self, offset: usize) -> Result<()> {
        if offset > self.data.len() {
            Err(anyhow!("Seek beyond end of buffer"))
        } else {
            self.offset = offset;
            Ok(())
        }
    }

    /// Seeks to an offset relative to the current position
    pub fn seek_relative(&mut self, offset: isize) -> Result<()> {
        let new_offset = self
            .offset
            .checked_add_signed(offset)
            .ok_or_else(|| anyhow!("Invalid seek"))?;
        if new_offset > self.data.len() {
            Err(anyhow!("Seek beyond end of buffer"))
        } else {
            self.offset = new_offset;
            Ok(())
        }
    }

    /// Seeks to an offset later in the file relative to the current position
    pub fn seek_forward(&mut self, offset: usize) -> Result<()> {
        let new_offset = self
            .offset
            .checked_add(offset)
            .ok_or_else(|| anyhow!("Invalid seek"))?;
        if new_offset > self.data.len() {
            Err(anyhow!("Seek beyond end of buffer"))
        } else {
            self.offset = new_offset;
            Ok(())
        }
    }

    /// Seeks to an offset earlier in the file relative to the current position
    pub fn seek_backward(&mut self, offset: usize) -> Result<()> {
        self.offset = self
            .offset
            .checked_sub(offset)
            .ok_or_else(|| anyhow!("Seek beyond beginning of buffer"))?;
        Ok(())
    }

    /// Seeks to an offset relative to the end of the buffer
    pub fn seek_end(&mut self, offset: usize) -> Result<()> {
        self.offset = self
            .data
            .len()
            .checked_sub(offset)
            .ok_or_else(|| anyhow!("Seek beyond beginning of buffer"))?;
        Ok(())
    }

    /// Returns the number of bytes remaining in the buffer
    pub fn bytes_remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Gets the remaining data in the buffer
    pub fn get_remaining(&self) -> &[u8] {
        &self.data[self.offset..]
    }

    /// Returns the total length of the buffer
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Determines if the current position is at the end of the buffer
    pub fn at_end(&self) -> bool {
        self.offset == self.data.len()
    }

    /// Reads the specified number of bytes from the current position
    pub fn get_bytes(&mut self, count: usize) -> Result<&[u8]> {
        if count > self.bytes_remaining() {
            Err(anyhow!("Not enough bytes remaining in buffer"))
        } else {
            let result = &self.data[self.offset..self.offset + count];
            self.offset += count;
            Ok(result)
        }
    }

    /// Gets the specified range of bytes from the data
    pub fn get_at(&self, offset: usize, size: usize) -> Result<&[u8]> {
        if offset > self.data.len() {
            Err(anyhow!("Data beyond end of buffer"))
        } else if offset
            .checked_add(size)
            .ok_or_else(|| anyhow!("Invalid size"))?
            > self.data.len()
        {
            Err(anyhow!("Data beyond end of buffer"))
        } else {
            Ok(&self.data[offset..offset + size])
        }
    }

    /// Reads the next u8 from the buffer
    pub fn get_u8(&mut self) -> Result<u8> {
        let bytes = self.get_bytes(1)?;
        Ok(bytes[0])
    }

    /// Reads the next i8 from the buffer
    pub fn get_i8(&mut self) -> Result<i8> {
        let bytes = self.get_bytes(1)?;
        Ok(bytes[0] as i8)
    }

    /// Reads the next u16 from the buffer
    pub fn get_u16(&mut self) -> Result<u16> {
        let bytes = self.get_bytes(2)?;
        Ok(u16::from_be_bytes(bytes.try_into()?))
    }

    /// Reads the next i16 from the buffer
    pub fn get_i16(&mut self) -> Result<i16> {
        let bytes = self.get_bytes(2)?;
        Ok(i16::from_be_bytes(bytes.try_into()?))
    }

    /// Reads the next u24 from the buffer
    pub fn get_u24(&mut self) -> Result<u32> {
        let bytes = self.get_bytes(3)?;
        Ok(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    /// Reads the next i24 from the buffer
    pub fn get_i24(&mut self) -> Result<i32> {
        let bytes = self.get_bytes(3)?;
        Ok(i32::from_be_bytes([
            if bytes[0] & 0x80 != 0 { 0xff } else { 0 },
            bytes[0],
            bytes[1],
            bytes[2],
        ]))
    }

    /// Reads the next u32 from the buffer
    pub fn get_u32(&mut self) -> Result<u32> {
        let bytes = self.get_bytes(4)?;
        Ok(u32::from_be_bytes(bytes.try_into()?))
    }

    /// Reads the next i32 from the buffer
    pub fn get_i32(&mut self) -> Result<i32> {
        let bytes = self.get_bytes(4)?;
        Ok(i32::from_be_bytes(bytes.try_into()?))
    }

    /// Reads the next variable length integer from the buffer
    pub fn get_var_int(&mut self) -> Result<u32> {
        let mut result = 0u32;
        loop {
            let byte = self.get_u8()?;
            result |= (byte & 0x7f) as u32;
            if byte & 0x80 == 0 {
                break;
            }
            result <<= 7;
        }
        Ok(result)
    }

    /// Reads a null-terminated string from the buffer
    pub fn get_string(&mut self) -> Result<String> {
        let mut chars = Vec::new();
        loop {
            let ch = self.get_u8()?;
            if ch == 0 {
                break;
            }
            chars.push(ch);
        }
        Ok(String::from_utf8(chars)?)
    }
}

impl OutputBuffer {
    pub fn new() -> Self {
        OutputBuffer { data: Vec::new() }
    }

    pub fn write_bytes(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    pub fn write_u8(&mut self, value: u8) {
        self.data.push(value);
    }

    pub fn write_i8(&mut self, value: i8) {
        self.write_u8(value as u8);
    }

    pub fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i16(&mut self, value: i16) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_u24(&mut self, value: u32) {
        self.write_bytes(&value.to_be_bytes()[1..4]);
    }

    pub fn write_i24(&mut self, value: i32) {
        self.write_bytes(&value.to_be_bytes()[1..4]);
    }

    pub fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_i32(&mut self, value: i32) {
        self.write_bytes(&value.to_be_bytes());
    }

    pub fn write_var_int(&mut self, mut value: u32) {
        let mut bytes = Vec::new();
        let orig_value = value;
        loop {
            bytes.push((value & 0x7f) as u8);
            value >>= 7;
            if value == 0 {
                break;
            }
        }
        bytes.reverse();
        for i in 0..bytes.len() - 1 {
            bytes[i] |= 0x80;
        }
        self.write_bytes(&bytes);
    }

    pub fn data(self) -> Vec<u8> {
        self.data
    }
}
