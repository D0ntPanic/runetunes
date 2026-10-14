mod archive;
mod buffer;
mod cache;
mod data;
mod instrument;
mod song;

use anyhow::Result;
use archive::Archive;
use cache::Cache;
use clap::Parser;
use data::Data;
use song::Song;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Args {
    path: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cache = Cache::new(&args.path)?;
    let data = Data::new(cache)?;
    std::fs::create_dir_all("output")?;

    let mut seen = std::collections::HashSet::new();

    for (song_id, name) in &data.song_names {
        if name.is_empty() || name == " " || name == "Rising Damp" {
            continue;
        }
        if let Some(file) = data.song_ids.get(song_id) {
            let song = Song::new(&data, *file)?;
            std::fs::write(
                Path::new("output").join(format!("{}.mid", name)),
                &song.as_midi()?,
            )?;

            for (i, track) in song.as_midi_per_track()?.into_iter().enumerate() {
                std::fs::write(
                    Path::new("output").join(format!("{}_track{}.mid", name, i)),
                    track,
                )?;
            }

            for (id, instrument) in &song.instruments {
                if seen.insert(*id) {
                    std::fs::write(
                        Path::new("output").join(format!("instrument_{}.dat", id)),
                        &instrument.data,
                    )?;
                }
            }
        }
    }

    Ok(())
}
