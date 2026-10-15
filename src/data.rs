use crate::archive::Archive;
use crate::audio::Engine;
use crate::buffer::Buffer;
use crate::cache::Cache;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

/// Old School configuration archive (DB tables live here).
pub const OSRS_CONFIG_ARCHIVE: usize = 2;
/// Archive holding procedural synth sounds (sound effects and some instrument samples).
pub const SYNTH_ARCHIVE: usize = 4;
/// Archive holding songs, one per group.
pub const SONG_ARCHIVE: usize = 6;
/// Archive holding Vorbis-compressed instrument samples.
pub const VORBIS_ARCHIVE: usize = 14;
/// Archive holding instrument patches, indexed by MIDI program (including bank bits).
pub const PATCH_ARCHIVE: usize = 15;
/// Build 530 archive holding client configuration ("enums"), including the music track list.
pub const BUILD530_CONFIG_ARCHIVE: usize = 17;

/// Old School config group holding DB table rows, one file per row.
const OSRS_DB_ROW_GROUP: usize = 38;
/// Old School DB table listing the music tracks.
const OSRS_MUSIC_TABLE: u32 = 44;
/// Columns of the music table.
const OSRS_MUSIC_DISPLAY_NAME: usize = 1;
const OSRS_MUSIC_MIDI: usize = 4;
/// DB column value type ID for strings (every other type is stored as a 32-bit integer).
const DB_TYPE_STRING: i32 = 36;

/// A song listed in the game's music player.
#[derive(Debug, Clone)]
pub struct SongEntry {
    /// Group ID in the song archive.
    pub group: u32,
    pub name: String,
}

pub struct Data {
    pub cache: Rc<Cache>,
    /// Which client version the cache belongs to, and therefore which music engine plays it.
    pub engine: Engine,
    pub song_archive: Archive,
    pub patch_archive: Archive,
    pub synth_archive: Archive,
    pub vorbis_archive: Archive,
    /// Named songs, in the order the game lists them. A group may appear more than once.
    pub songs: Vec<SongEntry>,
}

impl Data {
    /// Loads a cache, detecting which client version it belongs to unless `engine` is given.
    pub fn new(cache: Rc<Cache>, engine: Option<Engine>) -> Result<Self> {
        let song_archive = Archive::new(&cache, SONG_ARCHIVE)?;
        let patch_archive = Archive::new(&cache, PATCH_ARCHIVE)?;
        let synth_archive = Archive::new(&cache, SYNTH_ARCHIVE)?;
        let vorbis_archive = Archive::new(&cache, VORBIS_ARCHIVE)?;
        // Old School caches have used version 7 indexes since 2019; build 530 predates them.
        let engine = engine.unwrap_or(if song_archive.format() >= 7 {
            Engine::Osrs
        } else {
            Engine::Build530
        });
        let songs = if song_archive.format() >= 7 {
            osrs_songs(&cache)?
        } else {
            build530_songs(&cache)?
        };

        Ok(Data {
            cache,
            engine,
            song_archive,
            patch_archive,
            synth_archive,
            vorbis_archive,
            songs,
        })
    }
}

/// Build 530 lists songs in two enums of config group 5: names (file 65) and song groups
/// (file 71), both keyed by song ID.
fn build530_songs(cache: &Rc<Cache>) -> Result<Vec<SongEntry>> {
    let config = Archive::new(cache, BUILD530_CONFIG_ARCHIVE)?;
    let names = config.get_string_map(5, 65)?;
    let groups = config.get_u32_map(5, 71)?;
    Ok(names
        .into_iter()
        .filter_map(|(id, name)| groups.get(&id).map(|&group| SongEntry { group, name }))
        .collect())
}

/// Old School lists songs in the rows of DB table 44.
fn osrs_songs(cache: &Rc<Cache>) -> Result<Vec<SongEntry>> {
    let config = Archive::new(cache, OSRS_CONFIG_ARCHIVE)?;
    let mut songs = Vec::new();
    for (_, row) in config.get_all(OSRS_DB_ROW_GROUP)? {
        let row = DbRow::decode(row)?;
        if row.table != Some(OSRS_MUSIC_TABLE) {
            continue;
        }
        let name = row.columns.get(&OSRS_MUSIC_DISPLAY_NAME);
        let midi = row.columns.get(&OSRS_MUSIC_MIDI);
        if let (Some([DbValue::String(name), ..]), Some([DbValue::Int(group), ..])) =
            (name.map(Vec::as_slice), midi.map(Vec::as_slice))
        {
            if *group >= 0 {
                songs.push(SongEntry {
                    group: *group as u32,
                    name: name.clone(),
                });
            }
        }
    }
    Ok(songs)
}

#[derive(Debug, Clone)]
enum DbValue {
    Int(i32),
    String(String),
}

/// One row of a DB table: the table it belongs to and its column values.
struct DbRow {
    table: Option<u32>,
    columns: BTreeMap<usize, Vec<DbValue>>,
}

impl DbRow {
    fn decode(data: Vec<u8>) -> Result<Self> {
        let mut data = Buffer::new(data);
        let mut row = DbRow {
            table: None,
            columns: BTreeMap::new(),
        };
        loop {
            match data.get_u8()? {
                0 => break,
                3 => {
                    data.get_u8()?; // column count
                    loop {
                        let column = data.get_u8()?;
                        if column == 255 {
                            break;
                        }
                        let types = (0..data.get_u8()?)
                            .map(|_| data.get_unsigned_smart())
                            .collect::<Result<Vec<_>>>()?;
                        let mut values = Vec::new();
                        for _ in 0..data.get_unsigned_smart()? {
                            for &value_type in &types {
                                values.push(if value_type == DB_TYPE_STRING {
                                    DbValue::String(data.get_string()?)
                                } else {
                                    DbValue::Int(data.get_i32()?)
                                });
                            }
                        }
                        row.columns.insert(column as usize, values);
                    }
                }
                4 => row.table = Some(data.get_var_int_le()?),
                opcode => return Err(anyhow!("Unknown DB row opcode {}", opcode)),
            }
        }
        Ok(row)
    }
}

/// Songs as (group ID, name), sorted by name. Duplicate groups and placeholder entries are
/// skipped.
pub fn song_list(data: &Data) -> Vec<(u32, String)> {
    let mut seen = HashSet::new();
    let mut songs: Vec<(u32, String)> = data
        .songs
        .iter()
        .filter(|song| !song.name.trim().is_empty())
        .filter(|song| seen.insert(song.group))
        .map(|song| (song.group, song.name.clone()))
        .collect();
    songs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    songs
}
