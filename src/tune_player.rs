//! QuickTime Music Architecture tune player.
//!
//! A `'tune'` component sequences a stream of QTMA events -- notes, rests,
//! controllers and per-part instrument requests -- and renders them. The event
//! encoding is a packed bit layout rather than a byte structure, and every
//! field position below is from Apple's `QuickTimeMusic.h` (Universal
//! Interfaces 3.3.1), not inferred from the data.
//!
//! The synthesiser here is deliberately plain: a short harmonic series per
//! General MIDI family, an attack and either a plucked decay or a sustain with
//! release. It is not trying to sound like a QuickTime instrument, only to
//! play the right notes at the right times with a timbre that suits the part.
//! Durations in the stream are milliseconds and pitches are MIDI key numbers,
//! so neither needs a mapping table.

use crate::sound::{StereoSample, OUTPUT_RATE};

/// End of a tune stream. `QuickTimeMusic.h`, `kEndMarkerValue`.
///
/// This value is only a terminator when it falls on an event boundary. It
/// occurs freely as an operand inside multi-long events, so a reader that
/// scans for the word rather than walking events truncates the tune: the
/// first such word in Cythera's theme sits 1,952 bytes in, where the piece
/// runs to 8,408.
pub(crate) const END_MARKER_VALUE: u32 = 0x6000_0000;

// Event header fields, shared by every event type.
const EVENT_LENGTH_POS: u32 = 30;
const EVENT_LENGTH_WIDTH: u32 = 2;
const EVENT_TYPE_POS: u32 = 29;
const EVENT_TYPE_WIDTH: u32 = 3;
const X_EVENT_TYPE_POS: u32 = 28;
const X_EVENT_TYPE_WIDTH: u32 = 4;
const EVENT_PART_POS: u32 = 24;
const EVENT_PART_WIDTH: u32 = 5;
const X_EVENT_PART_POS: u32 = 16;
const X_EVENT_PART_WIDTH: u32 = 12;

// Short event types (`kEventTypeField` <= 3).
const REST_EVENT_TYPE: u32 = 0;
const NOTE_EVENT_TYPE: u32 = 1;
const CONTROL_EVENT_TYPE: u32 = 2;
const MARKER_EVENT_TYPE: u32 = 3;
// Extended types, read from `kXEventTypeField` when the short type is > 3.
const X_NOTE_EVENT_TYPE: u32 = 0x9;
const GENERAL_EVENT_TYPE: u32 = 0xF;

const REST_DURATION_POS: u32 = 0;
const REST_DURATION_WIDTH: u32 = 24;
const NOTE_PITCH_POS: u32 = 18;
const NOTE_PITCH_WIDTH: u32 = 6;
/// A short note event stores pitch relative to this. `kNoteEventPitchOffset`.
const NOTE_PITCH_OFFSET: u32 = 32;
const NOTE_VOLUME_POS: u32 = 11;
const NOTE_VOLUME_WIDTH: u32 = 7;
const NOTE_DURATION_POS: u32 = 0;
const NOTE_DURATION_WIDTH: u32 = 11;
const X_NOTE_PITCH_POS: u32 = 0;
const X_NOTE_PITCH_WIDTH: u32 = 16;
const X_NOTE_DURATION_POS: u32 = 0;
const X_NOTE_DURATION_WIDTH: u32 = 22;
const X_NOTE_VOLUME_POS: u32 = 22;
const X_NOTE_VOLUME_WIDTH: u32 = 7;
const CONTROL_CONTROLLER_POS: u32 = 16;
const CONTROL_CONTROLLER_WIDTH: u32 = 8;
const CONTROL_VALUE_POS: u32 = 0;
const CONTROL_VALUE_WIDTH: u32 = 16;
const MARKER_SUBTYPE_POS: u32 = 16;
const MARKER_SUBTYPE_WIDTH: u32 = 8;
const MARKER_VALUE_POS: u32 = 0;
const MARKER_VALUE_WIDTH: u32 = 16;
const GENERAL_SUBTYPE_POS: u32 = 16;
const GENERAL_SUBTYPE_WIDTH: u32 = 14;
const GENERAL_LENGTH_POS: u32 = 0;
const GENERAL_LENGTH_WIDTH: u32 = 16;

const MARKER_EVENT_END: u32 = 0;
const GENERAL_EVENT_NOTE_REQUEST: u32 = 1;

/// QTMA controller numbers, which are *not* MIDI controller numbers.
const CONTROLLER_VOLUME: u32 = 7;
const CONTROLLER_PART_VOLUME: u32 = 42;

/// The longest stream this will walk, in longs. A tune is a guest pointer with
/// no length, terminated by an end marker; a corrupt or unterminated stream
/// must not become an unbounded read.
pub(crate) const MAX_TUNE_LONGS: usize = 256 * 1024;

/// The longest rendered tune, in seconds. Cythera's longest is about 131 s.
const MAX_RENDER_SECONDS: u32 = 300;

/// Convert a duration in the tune's time-scale units to milliseconds.
///
/// `TuneSetTimeScale` gives the units per second. QuickTime's usual value, and
/// the one Cythera asks for, is 600 -- so treating a unit as a millisecond
/// plays everything 1.67 times too fast.
pub(crate) fn units_to_ms(units: u32, time_scale: u32) -> u32 {
    let scale = if time_scale == 0 { 600 } else { time_scale };
    ((u64::from(units) * 1000) / u64::from(scale)).min(u64::from(u32::MAX)) as u32
}

fn extract(value: u32, pos: u32, width: u32) -> u32 {
    if width >= 32 {
        return value >> pos;
    }
    (value >> pos) & ((1u32 << width) - 1)
}

fn event_type(word: u32) -> u32 {
    let short = extract(word, EVENT_TYPE_POS, EVENT_TYPE_WIDTH);
    if short > 3 {
        extract(word, X_EVENT_TYPE_POS, X_EVENT_TYPE_WIDTH)
    } else {
        short
    }
}

/// How many longs this event occupies. A length field of 3 means the count
/// lives in the event's own general-length field, counted in longs.
pub(crate) fn event_length_longs(word: u32) -> usize {
    match extract(word, EVENT_LENGTH_POS, EVENT_LENGTH_WIDTH) {
        3 => extract(word, GENERAL_LENGTH_POS, GENERAL_LENGTH_WIDTH) as usize,
        2 => 2,
        _ => 1,
    }
}

/// One note. Times are in the tune's own time-scale units, not milliseconds:
/// a QTMA stream is authored against the scale its player is given through
/// `TuneSetTimeScale`, and Cythera asks for 600, so one unit is 1/600 s.
/// Converting too early is what makes a tune play fast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TuneNote {
    pub(crate) start_units: u32,
    pub(crate) duration_units: u32,
    pub(crate) part: u8,
    pub(crate) pitch: u8,
    pub(crate) volume: u8,
}

/// A decoded tune: its notes, how long it runs, and the General MIDI program
/// each part asked for through its note request.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecodedTune {
    pub(crate) notes: Vec<TuneNote>,
    pub(crate) duration_units: u32,
    pub(crate) programs: Vec<(u8, u8)>,
    pub(crate) part_volume: Vec<(u8, u8)>,
}

impl DecodedTune {
    /// Adopt instrument assignments from the tune's header.
    ///
    /// `TuneSetHeader` carries the note requests -- the General MIDI program
    /// each part asks for -- and the queued stream usually carries none. A
    /// player that reads only the stream therefore renders every part on
    /// program 0, an acoustic grand piano, whatever the tune asked for.
    /// Anything the stream does declare wins, because it comes later.
    pub(crate) fn adopt_header_programs(&mut self, header: &[(u8, u8)]) {
        for (part, program) in header {
            if !self.programs.iter().any(|(p, _)| p == part) {
                self.programs.push((*part, *program));
            }
        }
    }

    pub(crate) fn program_for(&self, part: u8) -> u8 {
        self.programs
            .iter()
            .find(|(p, _)| *p == part)
            .map_or(0, |(_, program)| *program)
    }

    fn volume_for(&self, part: u8) -> u8 {
        self.part_volume
            .iter()
            .find(|(p, _)| *p == part)
            .map_or(100, |(_, volume)| *volume)
    }
}

/// The General MIDI program a note request asks for.
///
/// `NoteRequest` is an 8-byte `NoteRequestInfo` followed by a 76-byte
/// `ToneDescription`, whose last long is `gmNumber`.
fn note_request_gm_number(payload: &[u32]) -> Option<u8> {
    const NOTE_REQUEST_INFO_LONGS: usize = 2;
    const TONE_DESCRIPTION_LONGS: usize = 19;
    if payload.len() < NOTE_REQUEST_INFO_LONGS + TONE_DESCRIPTION_LONGS {
        return None;
    }
    let gm = payload[NOTE_REQUEST_INFO_LONGS + TONE_DESCRIPTION_LONGS - 1];
    // 0..=127 are General MIDI instruments; 128 is the drum kit, which this
    // synthesiser has no samples for and treats as an ordinary part.
    if gm <= 128 {
        Some(gm.min(127) as u8)
    } else {
        None
    }
}

/// Walk a tune stream into notes.
///
/// Time advances only on rest events; notes carry their own duration and do
/// not move the cursor, which is why a chord is a run of note events with no
/// rest between them.
pub(crate) fn decode_tune(words: &[u32]) -> DecodedTune {
    let mut tune = DecodedTune::default();
    let mut now_units: u32 = 0;
    let mut index = 0usize;

    while index < words.len() {
        let word = words[index];
        if word == END_MARKER_VALUE {
            break;
        }
        let length = event_length_longs(word);
        if length == 0 || index + length > words.len() {
            break;
        }

        match event_type(word) {
            REST_EVENT_TYPE => {
                now_units = now_units
                    .saturating_add(extract(word, REST_DURATION_POS, REST_DURATION_WIDTH));
            }
            NOTE_EVENT_TYPE => {
                tune.notes.push(TuneNote {
                    start_units: now_units,
                    duration_units: extract(word, NOTE_DURATION_POS, NOTE_DURATION_WIDTH),
                    part: extract(word, EVENT_PART_POS, EVENT_PART_WIDTH) as u8,
                    pitch: (extract(word, NOTE_PITCH_POS, NOTE_PITCH_WIDTH) + NOTE_PITCH_OFFSET)
                        .min(127) as u8,
                    volume: extract(word, NOTE_VOLUME_POS, NOTE_VOLUME_WIDTH) as u8,
                });
            }
            X_NOTE_EVENT_TYPE if length >= 2 => {
                let second = words[index + 1];
                tune.notes.push(TuneNote {
                    start_units: now_units,
                    duration_units: extract(second, X_NOTE_DURATION_POS, X_NOTE_DURATION_WIDTH),
                    part: extract(word, X_EVENT_PART_POS, X_EVENT_PART_WIDTH) as u8,
                    pitch: extract(word, X_NOTE_PITCH_POS, X_NOTE_PITCH_WIDTH).min(127) as u8,
                    volume: extract(second, X_NOTE_VOLUME_POS, X_NOTE_VOLUME_WIDTH) as u8,
                });
            }
            CONTROL_EVENT_TYPE => {
                let controller = extract(word, CONTROL_CONTROLLER_POS, CONTROL_CONTROLLER_WIDTH);
                if controller == CONTROLLER_VOLUME || controller == CONTROLLER_PART_VOLUME {
                    let part = extract(word, EVENT_PART_POS, EVENT_PART_WIDTH) as u8;
                    // Controller values are 16-bit with the useful range in the
                    // high byte; MIDI-style 0..127 is the low seven bits of it.
                    let value = extract(word, CONTROL_VALUE_POS, CONTROL_VALUE_WIDTH);
                    let scaled = ((value >> 8) & 0x7F) as u8;
                    tune.part_volume.retain(|(p, _)| *p != part);
                    tune.part_volume.push((part, scaled));
                }
            }
            MARKER_EVENT_TYPE => {
                let subtype = extract(word, MARKER_SUBTYPE_POS, MARKER_SUBTYPE_WIDTH);
                let value = extract(word, MARKER_VALUE_POS, MARKER_VALUE_WIDTH);
                if subtype == MARKER_EVENT_END && value == 0 {
                    break;
                }
            }
            GENERAL_EVENT_TYPE if length >= 2 => {
                let last = words[index + length - 1];
                if extract(last, GENERAL_SUBTYPE_POS, GENERAL_SUBTYPE_WIDTH)
                    == GENERAL_EVENT_NOTE_REQUEST
                {
                    let part = extract(word, X_EVENT_PART_POS, X_EVENT_PART_WIDTH) as u8;
                    if let Some(gm) = note_request_gm_number(&words[index + 1..index + length - 1])
                    {
                        tune.programs.retain(|(p, _)| *p != part);
                        tune.programs.push((part, gm));
                    }
                }
            }
            _ => {}
        }

        index += length;
    }

    tune.duration_units = tune
        .notes
        .iter()
        .map(|note| note.start_units.saturating_add(note.duration_units))
        .max()
        .unwrap_or(now_units)
        .max(now_units);
    tune
}

/// Harmonic amplitudes and whether the part decays like a plucked string, by
/// General MIDI family. Coarse on purpose: the families differ enough that a
/// harpsichord line does not sound like an organ line, which is as far as this
/// goes.
/// The longest harmonic series `timbre` returns.
const MAX_HARMONICS: usize = 5;

/// How a part behaves over time, which matters more to recognition than the
/// exact harmonic amplitudes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Voicing {
    /// Struck or plucked: all decay from the attack, ignoring note length.
    Plucked,
    /// Bowed, blown or synthesised: hold, then release.
    Sustained,
    /// Sung. A slow vibrato and a soft attack are what make a tone read as a
    /// voice rather than a string pad; Cythera's theme carries its lead on
    /// General MIDI 54, Synth Voice.
    Vocal,
}

fn timbre(program: u8) -> (&'static [f32], Voicing) {
    match program {
        0..=7 => (&[1.0, 0.5, 0.25, 0.12, 0.06], Voicing::Plucked),
        8..=15 => (&[1.0, 0.3, 0.6, 0.2], Voicing::Plucked),
        16..=23 => (&[1.0, 0.8, 0.6, 0.5, 0.3], Voicing::Sustained),
        24..=31 => (&[1.0, 0.6, 0.35, 0.2, 0.1], Voicing::Plucked),
        32..=39 => (&[1.0, 0.7, 0.2, 0.1], Voicing::Plucked),
        40..=51 => (&[1.0, 0.5, 0.35, 0.25, 0.15], Voicing::Sustained),
        // Choir Aahs, Voice Oohs, Synth Voice. A vowel's energy sits in the
        // low harmonics with little above the fourth, which is what separates
        // it from the strings either side of it in the General MIDI order.
        52..=54 => (&[1.0, 0.62, 0.30, 0.10], Voicing::Vocal),
        55..=63 => (&[1.0, 0.7, 0.5, 0.35, 0.2], Voicing::Sustained),
        64..=79 => (&[1.0, 0.35, 0.15, 0.08], Voicing::Sustained),
        // Synth leads and pads. 85 (Lead 6, voice) and 91 (Pad 4, choir) are
        // vocal in name and in use.
        85 | 91 => (&[1.0, 0.62, 0.30, 0.10], Voicing::Vocal),
        80..=95 => (&[1.0, 0.45, 0.3, 0.2], Voicing::Sustained),
        _ => (&[1.0, 0.4, 0.25, 0.15], Voicing::Sustained),
    }
}

/// MIDI key number to frequency, A440 equal temperament.
fn pitch_hz(pitch: u8) -> f32 {
    440.0 * 2.0f32.powf((f32::from(pitch) - 69.0) / 12.0)
}

/// Sine lookup, so rendering a dense tune is table reads and adds rather than
/// millions of `sin` calls. A tune is queued from inside a trap handler while
/// the game waits, so this is the difference between an unnoticeable pause and
/// a multi-second stall: the longest of Cythera's tunes needs on the order of
/// a hundred million partial samples.
const SINE_TABLE_LEN: usize = 4096;

/// Vibrato rate for sung parts, in Hz. Around six is the usual human rate.
const VIBRATO_HZ: f32 = 5.5;

fn sine_table() -> &'static [f32; SINE_TABLE_LEN] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[f32; SINE_TABLE_LEN]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0f32; SINE_TABLE_LEN];
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = ((index as f32 / SINE_TABLE_LEN as f32) * std::f32::consts::TAU).sin();
        }
        table
    })
}

/// Render a decoded tune to `OUTPUT_RATE` unsigned 8-bit stereo, the format
/// the Sound Manager channels take. `volume` is a QuickDraw `Fixed` where
/// 0x0001_0000 is unity, which is what `TuneSetVolume` is given.
pub(crate) fn render_tune(
    tune: &DecodedTune,
    volume_fixed: u32,
    time_scale: u32,
) -> Vec<StereoSample> {
    if tune.notes.is_empty() {
        return Vec::new();
    }
    let rate = OUTPUT_RATE as f32;
    let tail_ms = 250u32;
    let total_ms = units_to_ms(tune.duration_units, time_scale)
        .saturating_add(tail_ms)
        .min(MAX_RENDER_SECONDS.saturating_mul(1000));
    let frames = ((total_ms as f32 / 1000.0) * rate) as usize;
    if frames == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0.0f32; frames];

    for note in &tune.notes {
        let frequency = pitch_hz(note.pitch);
        if frequency <= 0.0 || frequency > rate / 2.0 {
            continue;
        }
        let start_ms = units_to_ms(note.start_units, time_scale);
        let start = ((start_ms as f32 / 1000.0) * rate) as usize;
        if start >= frames {
            continue;
        }
        // A note rings a little past its written duration, as a real
        // instrument does; the release below is what fades it.
        let duration_ms = units_to_ms(note.duration_units, time_scale);
        let sounding_ms = duration_ms.saturating_add(tail_ms);
        let mut length = ((sounding_ms as f32 / 1000.0) * rate) as usize;
        if start + length > frames {
            length = frames - start;
        }
        if length < 8 {
            continue;
        }

        let program = tune.program_for(note.part);
        let (harmonics, voicing) = timbre(program);
        let harmonic_sum: f32 = harmonics.iter().sum::<f32>().max(1e-6);
        let sustain_frames = ((duration_ms as f32 / 1000.0) * rate) as usize;
        // A sung note starts far more gently than a struck one.
        let attack_seconds = if voicing == Voicing::Vocal { 0.045 } else { 0.008 };
        let attack = ((attack_seconds * rate) as usize).max(1);
        let part_volume = f32::from(tune.volume_for(note.part)) / 127.0;
        let master = (volume_fixed as f32 / 65536.0).clamp(0.0, 1.0);
        let gain =
            (f32::from(note.volume) / 127.0) * (0.4 + 0.6 * part_volume) * 0.28 * master;
        if gain <= 0.0 {
            continue;
        }

        // One phase accumulator per harmonic, stepped by the partial's
        // frequency. Phases are kept as table indices in fixed point.
        let table = sine_table();
        let mut phase = [0.0f32; MAX_HARMONICS];
        let mut step = [0.0f32; MAX_HARMONICS];
        let mut partials = 0usize;
        for (index, amplitude) in harmonics.iter().enumerate() {
            let partial = frequency * (index as f32 + 1.0);
            if partial > rate / 2.0 {
                break;
            }
            step[index] = partial / rate * SINE_TABLE_LEN as f32;
            phase[index] = 0.0;
            let _ = amplitude;
            partials += 1;
        }

        for frame in 0..length {
            // A slow, shallow vibrato, faded in over the first moments of the
            // note the way a singer does rather than applied from the attack.
            let vibrato = if voicing == Voicing::Vocal {
                let seconds = frame as f32 / rate;
                let depth = 0.006 * (seconds / 0.25).min(1.0);
                let phase = (seconds * VIBRATO_HZ * SINE_TABLE_LEN as f32) as usize;
                1.0 + depth * table[phase & (SINE_TABLE_LEN - 1)]
            } else {
                1.0
            };
            let mut sample = 0.0f32;
            for index in 0..partials {
                let position = phase[index] as usize & (SINE_TABLE_LEN - 1);
                sample += harmonics[index] * table[position];
                phase[index] += step[index] * vibrato;
                if phase[index] >= SINE_TABLE_LEN as f32 {
                    phase[index] -= SINE_TABLE_LEN as f32;
                }
            }
            sample /= harmonic_sum;

            let envelope = match voicing {
                Voicing::Plucked => (-(frame as f32) / (0.45 * rate)).exp(),
                _ if frame < sustain_frames => 1.0,
                // A voice releases more slowly than an instrument stopped by
                // its player.
                Voicing::Vocal => {
                    (-((frame - sustain_frames) as f32) / (0.22 * rate)).exp()
                }
                Voicing::Sustained => {
                    (-((frame - sustain_frames) as f32) / (0.12 * rate)).exp()
                }
            };
            let envelope = if frame < attack {
                envelope * (frame as f32 / attack as f32)
            } else {
                envelope
            };

            buffer[start + frame] += sample * envelope * gain;
        }
    }

    // Normalise to just under full scale so a dense tune does not clip and a
    // sparse one is still audible.
    let peak = buffer.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    let scale = if peak > 0.0 { 0.89 / peak } else { 0.0 };

    buffer
        .into_iter()
        .map(|sample| {
            let scaled = (sample * scale).clamp(-1.0, 1.0);
            let byte = (scaled * 127.0 + 128.0).round().clamp(0.0, 255.0) as u8;
            StereoSample {
                left: byte,
                right: byte,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest(ms: u32) -> u32 {
        (REST_EVENT_TYPE << EVENT_TYPE_POS) | ms
    }

    fn note(part: u32, pitch: u32, volume: u32, duration: u32) -> u32 {
        (NOTE_EVENT_TYPE << EVENT_TYPE_POS)
            | (part << EVENT_PART_POS)
            | ((pitch - NOTE_PITCH_OFFSET) << NOTE_PITCH_POS)
            | (volume << NOTE_VOLUME_POS)
            | duration
    }

    #[test]
    fn rests_advance_time_and_notes_do_not() {
        // Two notes with no rest between them start together -- that is how a
        // chord is written -- and the rest after them moves the cursor on.
        let words = [
            note(0, 60, 100, 500),
            note(0, 64, 100, 500),
            rest(500),
            note(0, 67, 100, 250),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&words);
        assert_eq!(tune.notes.len(), 3);
        assert_eq!(tune.notes[0].start_units, 0);
        assert_eq!(tune.notes[1].start_units, 0);
        assert_eq!(tune.notes[0].pitch, 60);
        assert_eq!(tune.notes[1].pitch, 64);
        assert_eq!(tune.notes[2].start_units, 500);
        assert_eq!(tune.notes[2].pitch, 67);
        assert_eq!(tune.duration_units, 750);
    }

    #[test]
    fn an_end_marker_inside_an_event_is_not_a_terminator() {
        // The bug this pins. kEndMarkerValue occurs freely as an operand, and
        // a reader that scans for the word instead of walking events stops
        // there. In Cythera's theme the first such word is 1,952 bytes into a
        // 8,408 byte piece, so the music ended after a fifth of itself.
        let general_with_marker_operand = [
            // A general event declaring three longs, whose payload happens to
            // contain the end marker value.
            (3u32 << EVENT_LENGTH_POS) | (GENERAL_EVENT_TYPE << X_EVENT_TYPE_POS) | 3,
            END_MARKER_VALUE,
            0,
            note(0, 60, 100, 250),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&general_with_marker_operand);
        assert_eq!(
            tune.notes.len(),
            1,
            "the note after the embedded marker must still be decoded"
        );
    }

    #[test]
    fn header_programs_fill_in_parts_the_stream_does_not_name() {
        // TuneSetHeader carries the note requests; the queued stream usually
        // declares none, so without this every part renders as program 0, an
        // acoustic grand piano.
        let mut tune = decode_tune(&[note(1, 60, 100, 250), END_MARKER_VALUE]);
        assert_eq!(tune.program_for(1), 0, "nothing declared yet");
        tune.adopt_header_programs(&[(1, 54), (2, 87)]);
        assert_eq!(tune.program_for(1), 54, "the header names part 1");
        assert_eq!(tune.program_for(2), 87);
    }

    #[test]
    fn a_program_in_the_stream_beats_the_header() {
        let mut tune = decode_tune(&[note(1, 60, 100, 250), END_MARKER_VALUE]);
        tune.programs.push((1, 20));
        tune.adopt_header_programs(&[(1, 54)]);
        assert_eq!(tune.program_for(1), 20);
    }

    #[test]
    fn general_midi_voices_are_sung_rather_than_bowed() {
        // 52..54 are Choir Aahs, Voice Oohs and Synth Voice, and sit between
        // the strings and the brass in the General MIDI order; Cythera's
        // theme carries its lead on 54.
        assert_eq!(timbre(54).1, Voicing::Vocal);
        assert_eq!(timbre(48).1, Voicing::Sustained, "strings are not sung");
        assert_eq!(timbre(0).1, Voicing::Plucked, "a piano is struck");
    }

    #[test]
    fn decoding_stops_at_the_end_marker_and_ignores_what_follows() {
        let words = [note(0, 60, 100, 100), END_MARKER_VALUE, note(0, 72, 100, 100)];
        assert_eq!(decode_tune(&words).notes.len(), 1);
    }

    #[test]
    fn an_unterminated_stream_stops_at_the_end_of_the_slice() {
        // A tune is a bare guest pointer, so a stream that never reaches its
        // end marker must stop at the caller's bound rather than run on.
        let words = [note(0, 60, 100, 100); 8];
        assert_eq!(decode_tune(&words).notes.len(), 8);
    }

    #[test]
    fn a_zero_length_event_does_not_spin() {
        // Length field 3 takes the count from the general-length field; zero
        // there would leave the cursor where it is.
        let words = [(3u32 << EVENT_LENGTH_POS) | (0xF << X_EVENT_TYPE_POS), rest(10)];
        let tune = decode_tune(&words);
        assert_eq!(tune.notes.len(), 0);
        assert_eq!(tune.duration_units, 0);
    }

    #[test]
    fn rendering_produces_audible_samples_centred_on_silence() {
        let words = [note(0, 60, 127, 500), rest(500), END_MARKER_VALUE];
        let tune = decode_tune(&words);
        let samples = render_tune(&tune, 0x0001_0000, 600);
        assert!(!samples.is_empty());
        // 8-bit Sound Manager samples are unsigned with 0x80 as silence, and a
        // rendered note must actually leave it.
        assert!(samples.iter().any(|s| s.left != 0x80));
    }

    #[test]
    fn durations_are_time_scale_units_not_milliseconds() {
        // The bug this pins: Cythera asks for a scale of 600, so 600 units is
        // one second. Reading units as milliseconds plays the music 1.67x too
        // fast and ends it early.
        assert_eq!(units_to_ms(600, 600), 1000);
        assert_eq!(units_to_ms(300, 600), 500);
        assert_eq!(units_to_ms(1000, 1000), 1000);
        // A player that was never given a scale falls back to QuickTime's
        // usual 600 rather than dividing by zero.
        assert_eq!(units_to_ms(600, 0), 1000);
    }

    #[test]
    fn a_slower_time_scale_renders_a_longer_buffer() {
        let words = [note(0, 60, 127, 600), rest(600), END_MARKER_VALUE];
        let tune = decode_tune(&words);
        let fast = render_tune(&tune, 0x0001_0000, 1200).len();
        let slow = render_tune(&tune, 0x0001_0000, 600).len();
        assert!(slow > fast, "a smaller scale means longer notes: {slow} vs {fast}");
    }

    #[test]
    fn a_tune_with_no_notes_renders_nothing() {
        let tune = decode_tune(&[END_MARKER_VALUE]);
        assert!(render_tune(&tune, 0x0001_0000, 600).is_empty());
    }

    #[test]
    fn zero_volume_renders_silence() {
        let words = [note(0, 60, 127, 500), END_MARKER_VALUE];
        let tune = decode_tune(&words);
        let samples = render_tune(&tune, 0, 600);
        assert!(samples.iter().all(|s| s.left == 0x80 && s.right == 0x80));
    }
}
