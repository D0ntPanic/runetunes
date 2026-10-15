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
                // Trailing bytes that don't form a whole entry are ignored (Old School clients
                // leave 1-byte index files for unused archives).
                let mut index_file = Buffer::new(index_file);
                let mut entries = Vec::new();
                while index_file.bytes_remaining() >= 6 {
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
            // Each chunk is 520 bytes: a header followed by data. Groups with IDs above 65535
            // have a 10 byte header with a 32-bit group ID instead of the usual 8 byte one.
            let (header_size, group_size) = if entry.group > 0xffff {
                (10, 4)
            } else {
                (8, 2)
            };
            let remaining = entry.length - result.len();
            let chunk_size = (520 - header_size).min(remaining);
            let start = chunk * 520;
            let header = self
                .data
                .get(start..start + header_size + chunk_size)
                .ok_or_else(|| anyhow!("Cache entry outside of the data file"))?;

            // Decode chunk header and validate it
            let expected_group = header[..group_size]
                .iter()
                .fold(0usize, |id, &byte| id << 8 | byte as usize);
            let fields = &header[group_size..];
            let expected_chunk_index = u16::from_be_bytes([fields[0], fields[1]]) as usize;
            let expected_archive = fields[5] as usize;
            if expected_group != entry.group
                || expected_archive != entry.archive
                || expected_chunk_index != chunk_index
            {
                return Err(anyhow::anyhow!("Invalid cache entry"));
            }

            // Seek to next chunk
            chunk = u32::from_be_bytes([0, fields[2], fields[3], fields[4]]) as usize;

            // Append chunk data to the result
            result.extend_from_slice(&header[header_size..]);
            chunk_index += 1;
        }

        // Decompress the data
        Self::unpack(result)
    }

    /// Unpacks a cached group's data
    fn unpack(data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        if data.is_empty() {
            // Empty file
            return Ok(Vec::new());
        }

        let mut data = Buffer::new(data);

        // Header includes compression type and compressed length
        let compression_type = data.get_u8()?;
        let _length = data.get_u32()?;

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
