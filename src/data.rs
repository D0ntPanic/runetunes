use crate::archive::Archive;
use crate::cache::Cache;
use anyhow::Result;
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Data {
    pub cache: Rc<Cache>,
    pub data_map_archive: Archive,
    pub song_archive: Archive,
    pub instruments_archive: Archive,
    pub song_names: BTreeMap<u32, String>,
    pub song_ids: BTreeMap<u32, u32>,
}

impl Data {
    pub fn new(cache: Rc<Cache>) -> Result<Self> {
        let data_map_archive = Archive::new(&cache, 17)?;
        let song_archive = Archive::new(&cache, 6)?;
        let instruments_archive = Archive::new(&cache, 15)?;
        let song_names = data_map_archive.get_string_map(5, 65)?;
        let song_ids = data_map_archive.get_u32_map(5, 71)?;

        Ok(Data {
            cache,
            data_map_archive,
            song_archive,
            instruments_archive,
            song_names,
            song_ids,
        })
    }
}
