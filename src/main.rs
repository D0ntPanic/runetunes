pub mod archive;
pub mod audio;
pub mod buffer;
pub mod cache;
pub mod data;
pub mod song;
pub mod wav;

use anyhow::{anyhow, Result};
use audio::renderer::MusicRenderer;
use audio::sample_bank::{SampleBank, SampleSource};
use audio::{Engine, PlaybackOptions};
use cache::Cache;
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use data::{song_list, Data};
use song::Song;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Extracts and plays the music from a RuneScape 2009 (build 530) or Old School RuneScape game
/// cache.
#[derive(Parser)]
struct Args {
    /// Path to the cache directory (containing main_file_cache.dat2 and the .idx files)
    cache: PathBuf,
    /// Music engine to emulate. By default it is detected from the cache.
    #[arg(long, global = true)]
    engine: Option<EngineArg>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum)]
enum EngineArg {
    /// RuneScape 2 build 530 (2009scape)
    Build530,
    /// Old School RuneScape
    Osrs,
}

impl From<EngineArg> for Engine {
    fn from(engine: EngineArg) -> Self {
        match engine {
            EngineArg::Build530 => Engine::Build530,
            EngineArg::Osrs => Engine::Osrs,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Export every song as MIDI (whole song and per track) plus the raw patch files it uses.
    /// This is the default when no command is given.
    Extract {
        /// Directory to write the files to
        #[arg(short, long, default_value = "output")]
        output: PathBuf,
    },
    /// List the songs in the cache with their group IDs
    List,
    /// Render a song to a 16-bit WAV file using the game's synthesizer
    Render {
        /// Song name (case-insensitive), song group ID in archive 6, or a path to a standard
        /// MIDI file to play with the game's instruments
        song: String,
        /// Output WAV file (defaults to the song name with a .wav extension)
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        playback: PlaybackArgs,
        /// Length to render in seconds. By default the song is rendered until it ends (and
        /// `--loop` requires a length).
        #[arg(long)]
        seconds: Option<f64>,
        /// Silence rendered after the song ends, in seconds
        #[arg(long, default_value_t = 1.0)]
        tail: f64,
        /// Write the raw 32-bit mix buffer (little-endian i32, interleaved) instead of a WAV
        /// file, for bit-exact comparison with the Java oracle
        #[arg(long, hide = true)]
        raw: bool,
    },
    /// Play a song on the default audio output device
    Play {
        /// Song name (case-insensitive), song group ID in archive 6, or a path to a standard
        /// MIDI file to play with the game's instruments
        song: String,
        #[command(flatten)]
        playback: PlaybackArgs,
    },
    /// Decode every instrument sample of one kind into a directory (for oracle comparison)
    #[command(hide = true)]
    DumpSamples {
        /// "vorbis" or "synth"
        source: String,
        output: PathBuf,
        #[arg(long, default_value_t = 22050)]
        rate: i32,
    },
}

#[derive(ClapArgs)]
struct PlaybackArgs {
    /// Synthesizer sample rate in Hz (default is 22050)
    #[arg(long, default_value_t = 22050)]
    rate: i32,
    /// Render in mono instead of stereo
    #[arg(long)]
    mono: bool,
    /// Music volume, 0-255
    #[arg(long, default_value_t = 128)]
    volume: i32,
    /// Restart the song when it ends
    #[arg(long = "loop")]
    looping: bool,
}

impl PlaybackArgs {
    fn options(&self) -> PlaybackOptions {
        PlaybackOptions {
            sample_rate: self.rate,
            stereo: !self.mono,
            volume: self.volume,
            looping: self.looping,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cache = Cache::new(&args.cache)?;
    let data = Data::new(cache, args.engine.map(Engine::from))?;

    match args.command.unwrap_or(Command::Extract {
        output: PathBuf::from("output"),
    }) {
        Command::Extract { output } => extract(&data, &output),
        Command::List => {
            for (group, name) in song_list(&data) {
                println!("{:5}  {}", group, name);
            }
            Ok(())
        }
        Command::Render {
            song,
            output,
            playback,
            seconds,
            tail,
            raw,
        } => {
            let (song, name) = load_song(&data, &song)?;
            if playback.looping && seconds.is_none() {
                return Err(anyhow!("--loop needs --seconds"));
            }
            let output = output.unwrap_or_else(|| PathBuf::from(format!("{}.wav", name)));
            render(
                &data,
                &song,
                &playback.options(),
                seconds,
                tail,
                raw,
                &output,
            )?;
            println!("Rendered \"{}\" to {}", name, output.display());
            Ok(())
        }
        Command::Play { song, playback } => {
            let (song, name) = load_song(&data, &song)?;
            println!("Playing \"{}\" (Ctrl-C to stop)", name);
            let renderer = MusicRenderer::for_song(&data, &song, &playback.options())?;
            audio::device::play(renderer)
        }
        Command::DumpSamples {
            source,
            output,
            rate,
        } => dump_samples(&data, &source, &output, rate),
    }
}

/// Loads a song given a MIDI file path, group ID or name. Returns the song and its name.
fn load_song(data: &Data, query: &str) -> Result<(Song, String)> {
    let path = Path::new(query);
    if path.is_file() {
        let song = Song::from_midi_file(&std::fs::read(path)?)?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| query.to_string());
        return Ok((song, name));
    }
    let (group, name) = find_song(data, query)?;
    Ok((Song::new(data, group)?, name))
}

/// Looks up a song by group ID or name.
fn find_song(data: &Data, query: &str) -> Result<(u32, String)> {
    let songs = song_list(data);
    if let Ok(group) = query.parse::<u32>() {
        let name = songs
            .iter()
            .find(|(id, _)| *id == group)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| format!("Song {}", group));
        return Ok((group, name));
    }
    songs
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(query))
        .ok_or_else(|| anyhow!("No song named \"{}\" (use the list command)", query))
}

/// Renders a song to a WAV file, or to raw 32-bit mix data.
fn render(
    data: &Data,
    song: &Song,
    options: &PlaybackOptions,
    seconds: Option<f64>,
    tail: f64,
    raw: bool,
    output: &Path,
) -> Result<()> {
    let mut renderer = MusicRenderer::for_song(data, song, options)?;
    let chunk_seconds = renderer.chunk_frames() as f64 / options.sample_rate as f64;
    let mut out = std::io::BufWriter::new(std::fs::File::create(output)?);
    let mut pcm = Vec::new();

    // With a fixed length render that many chunks; otherwise render until the song stops,
    // followed by `tail` seconds.
    let mut chunks_left = seconds.map(|seconds| (seconds / chunk_seconds).ceil() as u64);
    let mut tail_chunks = (tail / chunk_seconds).ceil() as u64;
    loop {
        match &mut chunks_left {
            Some(0) => break,
            Some(left) => *left -= 1,
            None => {
                if tail_chunks == 0 {
                    break;
                }
                if !renderer.synth.is_playing() {
                    tail_chunks -= 1;
                }
            }
        }
        let chunk = renderer.render_chunk();
        if raw {
            for sample in chunk {
                out.write_all(&sample.to_le_bytes())?;
            }
        } else {
            pcm.extend(chunk.iter().map(|&sample| audio::renderer::to_i16(sample)));
        }
    }
    if !raw {
        wav::write(
            &mut out,
            options.sample_rate as u32,
            renderer.channels() as u16,
            &pcm,
        )?;
    }
    out.flush()?;
    Ok(())
}

/// Exports MIDI files and raw patch data for every song.
fn extract(data: &Data, output: &Path) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let mut seen = HashSet::new();

    for entry in &data.songs {
        if entry.name.trim().is_empty() {
            continue;
        }
        let name = entry.name.replace(['/', '\\'], "-");
        let song = Song::new(data, entry.group)?;
        std::fs::write(output.join(format!("{}.mid", name)), song.as_midi()?)?;

        for (i, track) in song.as_midi_per_track()?.into_iter().enumerate() {
            std::fs::write(output.join(format!("{}_track{}.mid", name, i)), track)?;
        }

        for id in song.patch_usage().keys() {
            if seen.insert(*id) {
                // Some songs use patches missing from the cache (build 530's "Rising Damp" uses
                // patch 279); the game can't play those, but their MIDI is still exported.
                match data.patch_archive.get_by_id(*id as usize) {
                    Ok(patch) => {
                        std::fs::write(output.join(format!("instrument_{}.dat", id)), patch)?
                    }
                    Err(_) => eprintln!("\"{}\" uses patch {}, which is missing", name, id),
                }
            }
        }
    }

    Ok(())
}

/// Decodes every sample of one kind, writing each as a header (rate, loop start, loop end,
/// ping-pong flag, length; little-endian i32) followed by the samples: 8-bit for build 530,
/// little-endian 16-bit for Old School.
fn dump_samples(data: &Data, source: &str, output: &Path, rate: i32) -> Result<()> {
    let mut bank = SampleBank::new(data, rate);
    let (source, archive, first) = match source {
        "vorbis" => (SampleSource::Vorbis, &data.vorbis_archive, 1),
        "synth" => (SampleSource::Synth, &data.synth_archive, 0),
        _ => return Err(anyhow!("Unknown sample source \"{}\"", source)),
    };
    std::fs::create_dir_all(output)?;
    for id in first..archive.group_capacity() {
        if archive.file_capacity(id) == 0
            || (data.engine == Engine::Build530 && archive.file_capacity(id) != 1)
        {
            continue;
        }
        let sample = match bank.get(source, id as u32) {
            Ok(sample) => sample,
            Err(e) => {
                eprintln!("sample {}: {:#}", id, e);
                continue;
            }
        };
        let mut out = Vec::new();
        for value in [
            sample.sample_rate,
            sample.loop_start,
            sample.loop_end,
            sample.ping_pong as i32,
            sample.len(),
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        if data.engine.sixteen_bit_samples() {
            out.extend(sample.data.iter().flat_map(|value| value.to_le_bytes()));
        } else {
            out.extend(sample.data.iter().map(|&value| value as u8));
        }
        std::fs::write(output.join(format!("{}.bin", id)), out)?;
    }
    Ok(())
}
