//! Minimal WAV file writer.

use anyhow::Result;
use std::io::Write;

/// Writes 16-bit PCM samples (interleaved when `channels > 1`) as a WAV file.
pub fn write(out: &mut impl Write, sample_rate: u32, channels: u16, samples: &[i16]) -> Result<()> {
    let data_size = (samples.len() * 2) as u32;
    let block_align = channels * 2;
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data_size).to_le_bytes())?;
    out.write_all(b"WAVE")?;
    out.write_all(b"fmt ")?;
    out.write_all(&16u32.to_le_bytes())?; // Format chunk size
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&channels.to_le_bytes())?;
    out.write_all(&sample_rate.to_le_bytes())?;
    out.write_all(&(sample_rate * block_align as u32).to_le_bytes())?;
    out.write_all(&block_align.to_le_bytes())?;
    out.write_all(&16u16.to_le_bytes())?; // Bits per sample
    out.write_all(b"data")?;
    out.write_all(&data_size.to_le_bytes())?;
    for sample in samples {
        out.write_all(&sample.to_le_bytes())?;
    }
    Ok(())
}
