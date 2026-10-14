use crate::buffer::Buffer;
use anyhow::{anyhow, Result};
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

/// An entry for where to find a single group in the cache
struct CacheEntry {
    archive: usize,
    group: usize,
    length: usize,
    chunk: usize,
}

/// Holds the raw group data for all the archives in the cache
pub struct Cache {
    data: Vec<u8>,
    index: Vec<Vec<CacheEntry>>,
}

impl Cache {
    /// Opens the cache at the given path
    pub fn new(path: &Path) -> Result<Rc<Self>> {
        // Read the main data file into memory
        let data = std::fs::read(path.join("main_file_cache.dat2"))?;

        // Parse the index files into memory
        let mut index = Vec::new();
        for i in 0..256 {
            if let Ok(index_file) = std::fs::read(path.join(format!("main_file_cache.idx{}", i))) {
                // Index files are made up of 6 byte entries, with two 24-bit fields. The fields
                // contain the length of the group's data and the chunk index in the main data file.
                let mut index_file = Buffer::new(index_file);
                let mut entries = Vec::new();
                while !index_file.at_end() {
                    let length = index_file.get_u24()?;
                    let chunk = index_file.get_u24()?;
                    entries.push(CacheEntry {
                        archive: i as usize,
                        group: entries.len(),
                        length: length as usize,
                        chunk: chunk as usize,
                    });
                }
                index.push(entries);
            } else {
                index.push(Vec::new());
            }
        }

        Ok(Rc::new(Cache { data, index }))
    }

    /// Gets a group's data from an archive
    pub fn get(&self, archive: usize, group: usize) -> Result<Vec<u8>> {
        if let Some(archive) = self.index.get(archive) {
            if let Some(entry) = archive.get(group) {
                self.read_cache_entry(entry)
            } else {
                Err(anyhow!("Group not found in archive"))
            }
        } else {
            Err(anyhow!("Archive not valid"))
        }
    }

    /// Gets the number of groups in an archive
    pub fn archive_groups(&self, archive: usize) -> usize {
        if let Some(archive) = self.index.get(archive) {
            archive.len()
        } else {
            0
        }
    }

    /// Reads the data for a single cache entry
    fn read_cache_entry(&self, entry: &CacheEntry) -> Result<Vec<u8>> {
        // Seek to the first chunk of the group
        let mut result = Vec::new();
        let mut chunk_index = 0;
        let mut chunk = entry.chunk;

        // Repeat until the entire group is read
        while result.len() < entry.length {
            // Compute remaining length. If greater than the chunk size (512), read the entire
            // chunk's data.
            let remaining = entry.length - result.len();
            let chunk_size = 512.min(remaining);

            // Compute chunk offset in the main data file. Each chunk is 520 bytes (512 bytes
            // of data and an 8 byte header).
            let start = chunk * 520;

            // Decode chunk header and validate it
            let expected_group =
                u16::from_be_bytes([self.data[start], self.data[start + 1]]) as usize;
            let expected_archive = self.data[start + 7] as usize;
            let expected_chunk_index =
                u16::from_be_bytes([self.data[start + 2], self.data[start + 3]]) as usize;
            if expected_group != entry.group
                || expected_archive != entry.archive
                || expected_chunk_index != chunk_index
            {
                return Err(anyhow::anyhow!("Invalid cache entry"));
            }

            // Seek to next chunk
            chunk = u32::from_be_bytes([
                0,
                self.data[start + 4],
                self.data[start + 5],
                self.data[start + 6],
            ]) as usize;

            // Append chunk data to the result
            result.extend_from_slice(&self.data[start + 8..start + 8 + chunk_size]);
            chunk_index += 1;
        }

        // Decompress the data
        Self::unpack(result)
    }

    /// Unpacks a cached group's data
    fn unpack(data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        if data.len() == 0 {
            // Empty file
            return Ok(Vec::new());
        }

        let mut data = Buffer::new(data);

        // Header includes compression type and compressed length
        let compression_type = data.get_u8()?;
        let length = data.get_u32()?;

        if compression_type == 0 {
            // Uncompressed data doesn't need an uncompressed length, only the data follows
            Ok(data.get_remaining().to_vec())
        } else {
            // Compressed data also includes the uncompressed length
            let uncompressed_length = data.get_u32()?;
            let mut result = Vec::with_capacity(uncompressed_length as usize);

            // Decompress based on compression type
            if compression_type == 1 {
                // Bzip2 method doesn't include the bzip2 header and only contains a raw block of
                // compressed data. Add a header before decompressing.
                let mut headered = vec![0x42, 0x5a, 0x68, 0x39];
                headered.extend_from_slice(data.get_remaining());
                let mut decoder = bzip2_rs::DecoderReader::new(&headered[..]);
                decoder.read_to_end(&mut result)?;
            } else {
                let mut decoder = flate2::read::GzDecoder::new(data.get_remaining());
                decoder.read_to_end(&mut result)?;
            }
            Ok(result)
        }
    }
}
