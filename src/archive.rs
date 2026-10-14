use crate::buffer::Buffer;
use crate::cache::Cache;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Archive {
    cache: Rc<Cache>,
    archive: usize,
    group_file_ids: BTreeMap<usize, Vec<usize>>,
    groups_by_name: BTreeMap<u32, usize>,
    group_files_by_name: BTreeMap<usize, BTreeMap<u32, usize>>,
}

impl Archive {
    /// Loads an archive from the cache
    pub fn new(cache: &Rc<Cache>, archive: usize) -> Result<Self> {
        // Index for each archive is stored in the cache within archive 255
        let mut index = Buffer::new(cache.get(255, archive)?);

        // First is a format indicator
        let format = index.get_u8()?;
        if format != 5 && format != 6 {
            return Err(anyhow!("Invalid index format"));
        }
        if format == 6 {
            // Format version 6 has an additional version field used by the downloader
            index.get_u32()?;
        }

        // Read flag for the presence of filenames
        let has_filenames = index.get_u8()? != 0;

        // Get index size
        let size = index.get_u16()?;

        // Read group IDs for the archive
        let mut group_ids = Vec::new();
        let mut current_id = 0;
        for _ in 0..size {
            current_id += index.get_u16()? as usize;
            group_ids.push(current_id);
        }

        // Read group names if present
        let mut groups_by_name = BTreeMap::new();
        if has_filenames {
            for id in &group_ids {
                groups_by_name.insert(index.get_u32()?, *id);
            }
        }

        // Read checksums and versions
        for _ in &group_ids {
            index.get_u32()?;
        }
        for _ in &group_ids {
            index.get_u32()?;
        }

        // Read group sizes. This is the number of files in each group.
        let mut group_sizes = Vec::new();
        for _ in &group_ids {
            group_sizes.push(index.get_u16()? as usize);
        }

        // Read file IDs for each group
        let mut group_file_ids = BTreeMap::new();
        for (idx, id) in group_ids.iter().enumerate() {
            let mut file_ids = Vec::new();
            let mut file_id = 0;
            for _ in 0..group_sizes[idx] {
                file_id += index.get_u16()? as usize;
                file_ids.push(file_id);
            }
            group_file_ids.insert(*id, file_ids);
        }

        // Read file names if present
        let mut group_files_by_name = BTreeMap::new();
        if has_filenames {
            for id in &group_ids {
                let mut files_by_name = BTreeMap::new();
                for file_id in &group_file_ids[id] {
                    let file_name = index.get_u32()?;
                    files_by_name.insert(file_name, *file_id);
                }
                group_files_by_name.insert(*id, files_by_name);
            }
        }

        Ok(Archive {
            cache: cache.clone(),
            archive,
            group_file_ids,
            groups_by_name,
            group_files_by_name,
        })
    }

    /// Gets the list of file IDs for the specified group
    fn file_ids(&self, group: usize) -> Result<&[usize]> {
        Ok(self
            .group_file_ids
            .get(&group)
            .ok_or_else(|| anyhow!("Group not found"))?)
    }

    /// Gets a file from the archive
    pub fn get(&self, group: usize, file_id: usize) -> Result<Vec<u8>> {
        // Get the data for the specified group from the cache
        let data = self.cache.get(self.archive, group)?;

        // Get the list of files in the group
        let file_ids = self.file_ids(group)?;

        // Look for the correct file ID in the group
        let file_idx = self
            .file_ids(group)?
            .iter()
            .position(|id| *id == file_id)
            .ok_or_else(|| anyhow!("File not found in group"))?;

        if file_ids.len() == 1 {
            // There is only one file in the group, so the file is the entire data
            return Ok(data.to_vec());
        }

        // More than one file in the group. The end of the group's data contains metadata about
        // the files within the group.
        let mut data = Buffer::new(data);
        data.seek_end(1)?;
        let stripes = data.get_u8()? as usize;
        data.seek_backward(1 + file_ids.len() * stripes * 4)?;

        // Read the sizes for each file in the group and gather the file data. Each i32 in the
        // metadata is the relative size of the file from the previous file.
        let mut result = Vec::new();
        let mut size = 0u32;
        let mut offset = 0;
        for _ in 0..stripes {
            for idx in 0..file_ids.len() {
                size = size
                    .checked_add_signed(data.get_i32()?)
                    .ok_or_else(|| anyhow!("Invalid file size"))?;
                if idx == file_idx {
                    result.extend_from_slice(&data.get_at(offset, size as usize)?);
                }
                offset += size as usize;
            }
        }

        Ok(result)
    }

    /// Gets the group ID for the given name
    pub fn group_id_by_name(&self, group: &str) -> Option<usize> {
        let mut hash = 0;
        for ch in group.as_bytes() {
            hash = (*ch as u32).wrapping_add(hash << 5).wrapping_sub(hash);
        }
        self.groups_by_name.get(&hash).map(|id| *id)
    }

    /// Gets a file using a group name
    pub fn get_by_group_name(&self, group: &str, file_id: usize) -> Result<Vec<u8>> {
        if let Some(group) = self.group_id_by_name(group) {
            self.get(group, file_id)
        } else {
            Err(anyhow!("Group name not found"))
        }
    }

    /// Gets a string map from the archive
    pub fn get_string_map(&self, group: usize, file_id: usize) -> Result<BTreeMap<u32, String>> {
        let mut data = Buffer::new(self.get(group, file_id)?);
        let mut result = BTreeMap::new();

        while !data.at_end() {
            match data.get_u8()? {
                0 => break,
                1 => {
                    // Key type
                    data.get_u8()?;
                }
                2 => {
                    // Value type
                    data.get_u8()?;
                }
                3 => {
                    // Default string
                    data.get_string()?;
                }
                5 => {
                    let size = data.get_u16()?;
                    for _ in 0..size {
                        let key = data.get_u32()?;
                        let value = data.get_string()?;
                        result.insert(key, value);
                    }
                }
                _ => return Err(anyhow!("Invalid map entry type")),
            }
        }

        Ok(result)
    }

    /// Gets an integer map from the archive
    pub fn get_u32_map(&self, group: usize, file_id: usize) -> Result<BTreeMap<u32, u32>> {
        let mut data = Buffer::new(self.get(group, file_id)?);
        let mut result = BTreeMap::new();

        while !data.at_end() {
            match data.get_u8()? {
                0 => break,
                1 => {
                    // Key type
                    data.get_u8()?;
                }
                2 => {
                    // Value type
                    data.get_u8()?;
                }
                4 => {
                    // Default integer
                    data.get_u32()?;
                }
                6 => {
                    let size = data.get_u16()?;
                    for _ in 0..size {
                        let key = data.get_u32()?;
                        let value = data.get_u32()?;
                        result.insert(key, value);
                    }
                }
                _ => return Err(anyhow!("Invalid map entry type")),
            }
        }

        Ok(result)
    }
}
