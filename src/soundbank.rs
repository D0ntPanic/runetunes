//! Names of the instruments in the game's soundbank.
//!
//! Patch numbers combine the MIDI bank and program the way [`crate::song::Song`] resolves them:
//! `bank * 128 + program`. The names come from the Old School RuneScape soundbank, which kept
//! the 2007 instruments; custom instruments in a build 530 cache may differ.

/// Returns the name of the instrument a patch number selects, if the soundbank has one.
pub fn patch_name(patch: u32) -> Option<&'static str> {
    let program = (patch & 0x7f) as u8;
    match patch >> 7 {
        0 => Some(STANDARD[program as usize]),
        1 => PERCUSSION
            .iter()
            .find(|(id, _)| *id == program)
            .map(|(_, name)| *name),
        2 => CUSTOM.get(program as usize).copied(),
        _ => None,
    }
}

/// General MIDI instruments, bank 0 (patches 0-127).
#[rustfmt::skip]
const STANDARD: [&str; 128] = [
    "Acoustic Grand Piano", // 0
    "Bright Acoustic Piano", // 1
    "Electric Grand Piano", // 2
    "Honky-tonk Piano", // 3
    "Electric Piano 1", // 4
    "Electric Piano 2", // 5
    "Harpsichord", // 6
    "Clavinet", // 7
    "Celesta", // 8
    "Glockenspiel", // 9
    "Music Box", // 10
    "Vibraphone", // 11
    "Marimba", // 12
    "Xylophone", // 13
    "Tubular Bells", // 14
    "Dulcimer", // 15
    "Drawbar Organ", // 16
    "Percussive Organ", // 17
    "Rock Organ", // 18
    "Church Organ", // 19
    "Reed Organ", // 20
    "Accordion", // 21
    "Harmonica", // 22
    "Tango Accordion", // 23
    "Acoustic Guitar (Nylon)", // 24
    "Acoustic Guitar (Steel)", // 25
    "Electric Guitar (Jazz)", // 26
    "Electric Guitar (Clean)", // 27
    "Electric Guitar (Muted)", // 28
    "Overdriven Guitar", // 29
    "Distortion Guitar", // 30
    "Guitar Harmonics", // 31
    "Acoustic Bass", // 32
    "Electric Bass (Finger)", // 33
    "Electric Bass (Pick)", // 34
    "Fretless Bass", // 35
    "Slap Bass 1", // 36
    "Slap Bass 2", // 37
    "Synth Bass 1", // 38
    "Synth Bass 2", // 39
    "Violin", // 40
    "Viola", // 41
    "Cello", // 42
    "Contrabass", // 43
    "Tremolo Strings", // 44
    "Pizzicato Strings", // 45
    "Orchestral Harp", // 46
    "Timpani", // 47
    "String Ensemble 1", // 48
    "String Ensemble 2 (Slow Strings)", // 49
    "Synth Strings 1", // 50
    "Synth Strings 2", // 51
    "Choir Aahs", // 52
    "Voice Oohs", // 53
    "Synth Voice", // 54
    "Orchestra Hit", // 55
    "Trumpet", // 56
    "Trombone", // 57
    "Tuba", // 58
    "Muted Trumpet", // 59
    "French Horn", // 60
    "Brass Section", // 61
    "Synth Brass 1", // 62
    "Synth Brass 2", // 63
    "Soprano Sax", // 64
    "Alto Sax", // 65
    "Tenor Sax", // 66
    "Baritone Sax", // 67
    "Oboe", // 68
    "English Horn", // 69
    "Bassoon", // 70
    "Clarinet", // 71
    "Piccolo", // 72
    "Flute", // 73
    "Recorder", // 74
    "Pan Flute", // 75
    "Blown Bottle", // 76
    "Shakuhachi", // 77
    "Whistle", // 78
    "Ocarina", // 79
    "Lead 1 (square)", // 80
    "Lead 2 (sawtooth)", // 81
    "Lead 3 (calliope)", // 82
    "Lead 4 (chiff)", // 83
    "Lead 5 (charang)", // 84
    "Lead 6 (voice)", // 85
    "Lead 7 (fifths)", // 86
    "Lead 8 (bass + lead)", // 87
    "Pad 1 (new age)", // 88
    "Pad 2 (warm)", // 89
    "Pad 3 (polysynth)", // 90
    "Pad 4 (choir)", // 91
    "Pad 5 (bowed)", // 92
    "Pad 6 (metallic)", // 93
    "Pad 7 (halo)", // 94
    "Pad 8 (sweep)", // 95
    "FX 1 (rain)", // 96
    "FX 2 (soundtrack)", // 97
    "FX 3 (crystal)", // 98
    "FX 4 (atmosphere)", // 99
    "FX 5 (brightness)", // 100
    "FX 6 (goblins)", // 101
    "FX 7 (echoes)", // 102
    "FX 8 (sci-fi)", // 103
    "Sitar", // 104
    "Banjo", // 105
    "Shamisen", // 106
    "Koto", // 107
    "Kalimba", // 108
    "Bag pipe", // 109
    "Fiddle", // 110
    "Shanai", // 111
    "Tinkle Bell", // 112
    "Agogo", // 113
    "Steel Drums", // 114
    "Woodblock", // 115
    "Taiko Drum", // 116
    "Melodic Tom", // 117
    "Synth Drum", // 118
    "Reverse Cymbal", // 119
    "Guitar Fret Noise", // 120
    "Breath Noise", // 121
    "Seashore", // 122
    "Bird Tweet", // 123
    "Telephone Ring", // 124
    "Helicopter", // 125
    "Applause", // 126
    "Gunshot", // 127
];

/// Percussion kits, bank 1 (patches 128-255), by program. Programs not listed have no kit.
#[rustfmt::skip]
const PERCUSSION: [(u8, &str); 16] = [
    (0, "Standard Drum Kit"),
    (1, "Gongs"),
    (2, "Ticks and Tocks"),
    (3, "Dark Drum"),
    (4, "Reverse Dark Drum"),
    (8, "Room Drum Kit"),
    (16, "Power Drum Kit"),
    (24, "Electronic Drum Kit"),
    (25, "Analog Drum Kit"),
    (40, "Brush Drum Kit"),
    (48, "Orchestral Drum Kit"),
    (50, "Ethnic Drum Kit"),
    (51, "Ethnic Drum Kit 2"),
    (56, "Special FX Drum Kit"),
    (57, "Impact Drum Kit"),
    (127, "Standard Drum Kit"),
];

/// Custom instruments, bank 2 (patches 256 onward).
#[rustfmt::skip]
const CUSTOM: [&str; 48] = [
    "Choir Ohhs", // 0
    "Opera Staccato", // 1
    "Opera Sustained", // 2
    "Choir Aahs 2", // 3
    "Synth Strings 2", // 4
    "Pizzicato Strings 2", // 5
    "Drum", // 6
    "Pan Flute 2", // 7
    "Tremolo Strings 2", // 8
    "Tremolo Strings 3", // 9
    "Echoes", // 10
    "Ambience", // 11
    "Overdriven Guitar 2", // 12
    "Ambience 2", // 13
    "Soft Tone", // 14
    "Synth Bass 3", // 15
    "Moo", // 16
    "Overdriven Guitar 3", // 17
    "Bells", // 18
    "Accordion", // 19
    "Quacks", // 20
    "Quacks 2", // 21
    "Overdriven Guitar 4", // 22
    "Quacks 3", // 23
    "Choir Aahs 3", // 24
    "Choir Aahs 4", // 25
    "Plink", // 26
    "Overdriven Guitar 5", // 27
    "Choir Ohhs 2", // 28
    "Overdriven Guitar 6", // 29
    "Organ", // 30
    "Dulcimer 2", // 31
    "Dulcimer 3", // 32
    "Dulcimer 4", // 33
    "Voice", // 34
    "Choir Ohhs 3", // 35
    "Deep Bass", // 36
    "Crystal 2", // 37
    "Guitar 2", // 38
    "Echoes 2", // 39
    "Drum 2", // 40
    "Drum 3", // 41
    "Choir Aahs 5", // 42
    "Choir Ohhs 4", // 43
    "Piano 2", // 44
    "Tremolo Strings 4", // 45
    "Throat Singing", // 46
    "Choir Aahs 6", // 47
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_by_bank() {
        assert_eq!(patch_name(0), Some("Acoustic Grand Piano"));
        assert_eq!(patch_name(127), Some("Gunshot"));
        assert_eq!(patch_name(128), Some("Standard Drum Kit"));
        assert_eq!(patch_name(128 + 56), Some("Special FX Drum Kit"));
        assert_eq!(patch_name(128 + 5), None);
        assert_eq!(patch_name(256 + 16), Some("Moo"));
        assert_eq!(patch_name(256 + 100), None);
        assert_eq!(patch_name(3 << 7), None);
    }
}
