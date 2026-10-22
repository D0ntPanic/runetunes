# Runetunes

Plays or extracts the music from a RuneScape game cache, such as the one shipped with
[2009scape](https://2009scape.org), or the one RuneLite keeps in
`~/.runelite/jagexcache/oldschool/LIVE`.

Besides exporting songs as MIDI, runetunes includes a Rust implementation of the game
client's synthesizer. Both versions of the engine are supported: Old School's descends from a
2007 snapshot of the same code and was later reworked for 16-bit audio. The implementation's
output has been verified to be bit-identical to the original Java code of both clients.

## Installation

This program requires a valid Rust toolchain. If you do not have one, you can get one from
the [official Rust site](https://rust-lang.org/).

To install, run the following command from the root of this repository:

```sh
cargo install --locked --path .
```

## Usage

```sh
runetunes play "Harmony"       # play on the default audio device
runetunes play my_song.mid     # play any MIDI file with the game's instruments
runetunes shuffle songs.txt    # play the songs listed in a file, shuffled, until Ctrl-C
runetunes extract              # export every song as MIDI (+ raw patch files) to ./output
runetunes extract -o DIR       # same, to DIR
runetunes list                 # list songs with their group IDs
runetunes render "Harmony"     # render a song to Harmony.wav
runetunes render 42 -o out.wav # songs can also be selected by group ID
```

The default is to play music out of the game cache stored by RuneLite. This can be overriden
using the `--cache` option specifying the cache directory containing `main_file_cache.dat2`
and the `.idx` files.

A `shuffle` playlist lists one song per line, by name or group ID; blank lines are ignored.
Each song plays once in a random order, and after that the order keeps reshuffling, but a song
never comes up again until at least a third of the list has played in between.

The engine version is detected from the cache. `--engine build530` or `--engine osrs`
overrides the engine version.

An enhanced engine is available with `--engine enhanced`. This fixes pitch issues at higher
sampling rates and defaults to 48kHz rendering. It also uses an improved resampling filter that
reduces artifacts, especially with low notes. Nothing else is changed, so songs keep the same
flavor, but sound cleaner.

Rendering and playback options:

| Option | Default | Meaning |
| --- | --- | --- |
| `--rate HZ` | 22050 | Synthesizer sample rate |
| `--mono` | off | Render in mono |
| `--volume V` | 128 | Music volume, 0-255 |
| `--loop` | off | Restart the song when it ends (`render` then needs `--seconds`) |
| `--seconds N` | whole song | `render` only: length to render |
| `--tail N` | 1 | `render` only: seconds of silence after the song ends |

If using the default engine that mirrors the original, many songs do not sound as you'd
expect with a non-default sample rate, but the results match the original implementation.
