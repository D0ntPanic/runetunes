use crate::buffer::{Buffer, OutputBuffer};
use crate::data::Data;
use crate::instrument::Instrument;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet};

pub struct Song {
    pub tracks: Vec<Track>,
    pub events: Vec<TrackEvent>,
    pub instruments: BTreeMap<u32, Instrument>,
    pub division: u16,
}

#[derive(Debug)]
pub struct Track {
    pub events: Vec<TrackEvent>,
}

#[derive(Debug, Clone)]
pub struct TrackEvent {
    pub track: usize,
    pub time: u32,
    pub event: Event,
}

#[derive(Debug, Clone)]
pub enum Event {
    EndOfTrack,
    TempoChange(u32),
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    Controller {
        channel: u8,
        controller: u8,
        value: u8,
    },
    PitchWheel {
        channel: u8,
        position: u16,
    },
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    KeyPressure {
        channel: u8,
        note: u8,
        pressure: u8,
    },
    BankSelect {
        channel: u8,
        bank: u8,
    },
    ProgramSelect {
        channel: u8,
        bank: u8,
        program: u32,
    },
}

impl Song {
    // Creates a new song from the given data
    pub fn new(data: &Data, id: u32) -> Result<Self> {
        // Get the song data from the archive
        let contents = data.song_archive.get(id as usize, 0)?;

        // At the end of the data is the track count and division
        let mut contents = Buffer::new(contents);
        contents.seek_end(3)?;
        let track_count = contents.get_u8()?;
        let division = contents.get_u16()?;
        contents.seek_absolute(0)?;

        // At the start of the data is the status and channel data for each track. Event data is
        // separate, so track the number of each event type.
        let mut status_and_channel_data = contents.clone();
        let mut tempo_changes = 0;
        let mut controller_events = 0;
        let mut note_on_events = 0;
        let mut note_off_events = 0;
        let mut pitch_wheel_events = 0;
        let mut channel_pressure_events = 0;
        let mut key_pressure_events = 0;
        let mut bank_select_events = 0;
        for _ in 0..track_count {
            loop {
                let status_and_channel = contents.get_u8()?;
                if status_and_channel == 7 {
                    break;
                } else if status_and_channel == 23 {
                    tempo_changes += 1;
                } else {
                    match status_and_channel & 0x0f {
                        0 => note_on_events += 1,
                        1 => note_off_events += 1,
                        2 => controller_events += 1,
                        3 => pitch_wheel_events += 1,
                        4 => channel_pressure_events += 1,
                        5 => key_pressure_events += 1,
                        6 => bank_select_events += 1,
                        _ => return Err(anyhow!("Invalid status: {}", status_and_channel)),
                    }
                }
            }
        }

        // Next is the delta time data. Save the cursor for later and seek past it.
        let mut delta_time_data = contents.clone();
        let delta_time_count = track_count as usize
            + tempo_changes
            + controller_events
            + note_on_events
            + note_off_events
            + pitch_wheel_events
            + channel_pressure_events
            + key_pressure_events
            + bank_select_events;
        for _ in 0..delta_time_count {
            contents.get_var_int()?;
        }

        // Next are the controller events. Save the cursor for later and gather the different
        // types of controller events, as the data for these is separate.
        let mut controller_event_data = contents.clone();
        let mut modulation_wheel_msb_events = 0;
        let mut modulation_wheel_lsb_events = 0;
        let mut channel_volume_msb_events = 0;
        let mut channel_volume_lsb_events = 0;
        let mut pan_msb_events = 0;
        let mut pan_lsb_events = 0;
        let mut non_registered_msb_events = 0;
        let mut non_registered_lsb_events = 0;
        let mut registered_msb_events = 0;
        let mut registered_lsb_events = 0;
        let mut other_known_controller_events = 0;
        let mut unknown_controller_events = 0;
        let mut controller = 0u8;
        for _ in 0..controller_events {
            controller = (controller.wrapping_add(contents.get_u8()?)) & 0x7f;
            match controller {
                0 | 32 => bank_select_events += 1,
                1 => modulation_wheel_msb_events += 1,
                33 => modulation_wheel_lsb_events += 1,
                7 => channel_volume_msb_events += 1,
                39 => channel_volume_lsb_events += 1,
                10 => pan_msb_events += 1,
                42 => pan_lsb_events += 1,
                99 => non_registered_msb_events += 1,
                98 => non_registered_lsb_events += 1,
                101 => registered_msb_events += 1,
                100 => registered_lsb_events += 1,
                64 | 65 | 120 | 121 | 123 => other_known_controller_events += 1,
                _ => unknown_controller_events += 1,
            }
        }

        // Calculate the position of each type of event data
        let mut other_known_controller_data = contents.clone();
        contents.seek_forward(other_known_controller_events)?;
        let mut key_pressure_data = contents.clone();
        contents.seek_forward(key_pressure_events)?;
        let mut channel_pressure_data = contents.clone();
        contents.seek_forward(channel_pressure_events)?;
        let mut pitch_wheel_msb_data = contents.clone();
        contents.seek_forward(pitch_wheel_events)?;
        let mut modulation_wheel_msb_data = contents.clone();
        contents.seek_forward(modulation_wheel_msb_events)?;
        let mut channel_volume_msb_data = contents.clone();
        contents.seek_forward(channel_volume_msb_events)?;
        let mut pan_msb_data = contents.clone();
        contents.seek_forward(pan_msb_events)?;
        let mut key_data = contents.clone();
        contents.seek_forward(note_on_events + note_off_events + key_pressure_events)?;
        let mut on_velocity_data = contents.clone();
        contents.seek_forward(note_on_events)?;
        let mut unknown_controller_data = contents.clone();
        contents.seek_forward(unknown_controller_events)?;
        let mut off_velocity_data = contents.clone();
        contents.seek_forward(note_off_events)?;
        let mut modulation_wheel_lsb_data = contents.clone();
        contents.seek_forward(modulation_wheel_lsb_events)?;
        let mut channel_volume_lsb_data = contents.clone();
        contents.seek_forward(channel_volume_lsb_events)?;
        let mut pan_lsb_data = contents.clone();
        contents.seek_forward(pan_lsb_events)?;
        let mut bank_select_data = contents.clone();
        contents.seek_forward(bank_select_events)?;
        let mut pitch_wheel_lsb_data = contents.clone();
        contents.seek_forward(pitch_wheel_events)?;
        let mut non_registered_msb_data = contents.clone();
        contents.seek_forward(non_registered_msb_events)?;
        let mut non_registered_lsb_data = contents.clone();
        contents.seek_forward(non_registered_lsb_events)?;
        let mut registered_msb_data = contents.clone();
        contents.seek_forward(registered_msb_events)?;
        let mut registered_lsb_data = contents.clone();
        contents.seek_forward(registered_lsb_events)?;
        let mut tempo_data = contents.clone();

        // Create tracks
        let mut tracks = Vec::new();
        for _ in 0..track_count {
            tracks.push(Track { events: Vec::new() });
        }

        // Gather timing data and output final event information for each track
        let mut channel = 0u8;
        let mut note = 0u8;
        let mut on_velocity = 0u8;
        let mut off_velocity = 0u8;
        let mut controller = 0u8;
        let mut controller_values = [0u8; 0x80];
        let mut pitch_wheel = 0u16;
        let mut channel_pressure = 0u8;
        let mut key_pressure = 0u8;
        for (track_idx, track) in tracks.iter_mut().enumerate() {
            let mut time = 0u32;
            loop {
                let delta_time = delta_time_data.get_var_int()?;
                time = time
                    .checked_add(delta_time)
                    .ok_or_else(|| anyhow!("Time overflow"))?;

                let status_and_channel = status_and_channel_data.get_u8()?;
                if status_and_channel == 7 {
                    track.events.push(TrackEvent {
                        track: track_idx,
                        time,
                        event: Event::EndOfTrack,
                    });
                    break;
                } else if status_and_channel == 23 {
                    track.events.push(TrackEvent {
                        track: track_idx,
                        time,
                        event: Event::TempoChange(tempo_data.get_u24()?),
                    });
                } else {
                    channel ^= status_and_channel >> 4;
                    match status_and_channel & 0x0f {
                        0 => {
                            note = note.wrapping_add(key_data.get_u8()?) & 0x7f;
                            on_velocity =
                                on_velocity.wrapping_add(on_velocity_data.get_u8()?) & 0x7f;
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::NoteOn {
                                    channel,
                                    note,
                                    velocity: on_velocity,
                                },
                            });
                        }
                        1 => {
                            note = note.wrapping_add(key_data.get_u8()?) & 0x7f;
                            off_velocity =
                                off_velocity.wrapping_add(off_velocity_data.get_u8()?) & 0x7f;
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::NoteOff {
                                    channel,
                                    note,
                                    velocity: off_velocity,
                                },
                            });
                        }
                        2 => {
                            controller =
                                (controller.wrapping_add(controller_event_data.get_u8()?)) & 0x7f;
                            let delta = match controller {
                                0 | 32 => bank_select_data.get_u8()?,
                                1 => modulation_wheel_msb_data.get_u8()?,
                                33 => modulation_wheel_lsb_data.get_u8()?,
                                7 => channel_volume_msb_data.get_u8()?,
                                39 => channel_volume_lsb_data.get_u8()?,
                                10 => pan_msb_data.get_u8()?,
                                42 => pan_lsb_data.get_u8()?,
                                99 => non_registered_msb_data.get_u8()?,
                                98 => non_registered_lsb_data.get_u8()?,
                                101 => registered_msb_data.get_u8()?,
                                100 => registered_lsb_data.get_u8()?,
                                64 | 65 | 120 | 121 | 123 => {
                                    other_known_controller_data.get_u8()?
                                }
                                _ => unknown_controller_data.get_u8()?,
                            };
                            controller_values[controller as usize] =
                                controller_values[controller as usize].wrapping_add(delta) & 0x7f;
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::Controller {
                                    channel,
                                    controller,
                                    value: controller_values[controller as usize],
                                },
                            });
                        }
                        3 => {
                            pitch_wheel =
                                pitch_wheel.wrapping_add(pitch_wheel_lsb_data.get_u8()? as u16);
                            pitch_wheel = pitch_wheel
                                .wrapping_add((pitch_wheel_msb_data.get_u8()? as u16) << 7);
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::PitchWheel {
                                    channel,
                                    position: pitch_wheel & 0x3fff,
                                },
                            })
                        }
                        4 => {
                            channel_pressure = channel_pressure
                                .wrapping_add(channel_pressure_data.get_u8()?)
                                & 0x7f;
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::ChannelPressure {
                                    channel,
                                    pressure: channel_pressure,
                                },
                            });
                        }
                        5 => {
                            note = note.wrapping_add(key_data.get_u8()?) & 0x7f;
                            key_pressure =
                                key_pressure.wrapping_add(key_pressure_data.get_u8()?) & 0x7f;
                            track.events.push(TrackEvent {
                                track: track_idx,
                                time,
                                event: Event::KeyPressure {
                                    channel,
                                    note,
                                    pressure: key_pressure,
                                },
                            })
                        }
                        6 => track.events.push(TrackEvent {
                            track: track_idx,
                            time,
                            event: Event::BankSelect {
                                channel,
                                bank: bank_select_data.get_u8()?,
                            },
                        }),
                        _ => return Err(anyhow!("Invalid status: {}", status_and_channel)),
                    }
                }
            }
        }

        // Construct combined event list sorted by time
        let mut events = Vec::new();
        for track in &tracks {
            events.extend_from_slice(&track.events.clone());
        }
        events.sort_by(|a, b| a.time.cmp(&b.time).then(a.track.cmp(&b.track)));

        // Replace bank select events with program select events. Controller events are used to
        // change the high bits of the program number.
        let mut program_base = [0u32; 16];
        let mut program = [0u32; 16];
        program_base[9] = 0x80;
        program[9] = 0x80;
        let mut instruments_used = BTreeSet::new();
        for event in events.iter_mut() {
            match event.event {
                Event::Controller {
                    channel,
                    controller,
                    value,
                    ..
                } => {
                    if controller == 0 {
                        program_base[channel as usize] =
                            (program_base[channel as usize] & !(0x7f << 14)) | (value as u32) << 14;
                    } else if controller == 32 {
                        program_base[channel as usize] =
                            (program_base[channel as usize] & !(0x7f << 7)) | (value as u32) << 7;
                    }
                }
                Event::BankSelect { channel, bank } => {
                    program[channel as usize] = program_base[channel as usize] | (bank as u32);
                    event.event = Event::ProgramSelect {
                        channel,
                        bank,
                        program: program[channel as usize],
                    };
                }
                Event::NoteOn { channel, .. } => {
                    instruments_used.insert(program[channel as usize]);
                }
                _ => (),
            }
        }

        // Load all the instruments used in the song
        let mut instruments = BTreeMap::new();
        for instrument_idx in instruments_used {
            instruments.insert(instrument_idx, Instrument::new(data, instrument_idx)?);
        }

        Ok(Song {
            tracks,
            events,
            instruments,
            division,
        })
    }

    fn track_midi(&self, track: &Track) -> Result<Vec<u8>> {
        let mut out = OutputBuffer::new();

        // Write each event in the track
        let mut time = 0;
        for event in &track.events {
            // Write delta time for event
            let delta_time = event
                .time
                .checked_sub(time)
                .ok_or_else(|| anyhow!("Events out of order"))?;
            out.write_var_int(delta_time);
            time = event.time;

            // Write event data
            match event.event {
                Event::EndOfTrack => {
                    out.write_u8(0xff);
                    out.write_u8(0x2f);
                    out.write_u8(0x00);
                    break;
                }
                Event::TempoChange(tempo) => {
                    out.write_u8(0xff);
                    out.write_u8(0x51);
                    out.write_u8(0x03);
                    out.write_u24(tempo);
                }
                Event::NoteOn {
                    channel,
                    note,
                    velocity,
                } => {
                    out.write_u8(0x90 | channel);
                    out.write_u8(note);
                    out.write_u8(velocity);
                }
                Event::NoteOff {
                    channel,
                    note,
                    velocity,
                } => {
                    out.write_u8(0x80 | channel);
                    out.write_u8(note);
                    out.write_u8(velocity);
                }
                Event::Controller {
                    channel,
                    controller,
                    value,
                } => {
                    out.write_u8(0xb0 | channel);
                    out.write_u8(controller);
                    out.write_u8(value);
                }
                Event::PitchWheel { channel, position } => {
                    out.write_u8(0xe0 | channel);
                    out.write_u8((position & 0x7f) as u8);
                    out.write_u8(((position >> 7) & 0x7f) as u8);
                }
                Event::ChannelPressure { channel, pressure } => {
                    out.write_u8(0xd0 | channel);
                    out.write_u8(pressure);
                }
                Event::KeyPressure {
                    channel,
                    note,
                    pressure,
                } => {
                    out.write_u8(0xa0 | channel);
                    out.write_u8(note);
                    out.write_u8(pressure);
                }
                Event::BankSelect { channel, bank } => {
                    out.write_u8(0xc0 | channel);
                    out.write_u8(bank);
                }
                Event::ProgramSelect { channel, bank, .. } => {
                    out.write_u8(0xc0 | channel);
                    out.write_u8(bank);
                }
            }
        }

        Ok(out.data())
    }

    /// Convert the song to MIDI format
    pub fn as_midi(&self) -> Result<Vec<u8>> {
        let mut out = OutputBuffer::new();

        // Write header
        out.write_bytes("MThd".as_bytes());
        out.write_u32(6); // Header size
        out.write_u16(if self.tracks.len() > 1 { 1 } else { 0 }); // Format
        out.write_u16(self.tracks.len() as u16);
        out.write_u16(self.division);

        // Write each track
        for track in &self.tracks {
            let track_out = self.track_midi(track)?;
            out.write_bytes("MTrk".as_bytes());
            out.write_u32(track_out.len() as u32);
            out.write_bytes(&track_out);
        }

        Ok(out.data())
    }

    /// Convert the song to MIDI format
    pub fn as_midi_per_track(&self) -> Result<Vec<Vec<u8>>> {
        let mut result = Vec::new();

        // First track often contains just the tempo for the song
        let mut skip_count = 1;
        let mut first_track_events: Vec<_> = self
            .tracks
            .get(0)
            .ok_or_else(|| anyhow!("No tracks found"))?
            .events
            .iter()
            .filter(|event| !matches!(event.event, Event::EndOfTrack))
            .cloned()
            .collect();

        if first_track_events.len() != 1 {
            first_track_events.clear();
            skip_count = 0;
        }

        // Write each track
        for track in self.tracks.iter().skip(skip_count) {
            let mut out = OutputBuffer::new();

            // Write header
            out.write_bytes("MThd".as_bytes());
            out.write_u32(6); // Header size
            out.write_u16(0); // Format
            out.write_u16(1);
            out.write_u16(self.division);

            let mut events = first_track_events.clone();
            events.extend_from_slice(&track.events);

            // Write track chunk to output
            let track_out = self.track_midi(&Track { events })?;
            out.write_bytes("MTrk".as_bytes());
            out.write_u32(track_out.len() as u32);
            out.write_bytes(&track_out);

            result.push(out.data());
        }

        Ok(result)
    }
}
