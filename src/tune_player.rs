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
/// `QuickTimeMusic.h`; the readings of each are delvmod's, in `delv/sound.py`,
/// which is the only other implementation of this decode.
const CONTROLLER_MODULATION: u32 = 1;
const CONTROLLER_VOLUME: u32 = 7;
const CONTROLLER_PAN: u32 = 10;
const CONTROLLER_PITCH_BEND: u32 = 32;
const CONTROLLER_AFTERTOUCH: u32 = 33;
const CONTROLLER_SUSTAIN: u32 = 64;
const CONTROLLER_PART_VOLUME: u32 = 42;

/// Extended control events, eight bytes, with a wider part and controller.
const X_CONTROL_EVENT_TYPE: u32 = 0xA;
/// In the second long: the top two bits are the length field, then a
/// fourteen-bit controller, then a sixteen-bit value.
const X_CONTROL_CONTROLLER_POS: u32 = 16;
const X_CONTROL_CONTROLLER_WIDTH: u32 = 14;
const X_CONTROL_VALUE_POS: u32 = 0;
const X_CONTROL_VALUE_WIDTH: u32 = 16;

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

/// Convert milliseconds back to the tune's time-scale units.
pub(crate) fn ms_to_units(ms: u32, time_scale: u32) -> u32 {
    let scale = if time_scale == 0 { 600 } else { time_scale };
    ((u64::from(ms) * u64::from(scale)) / 1000).min(u64::from(u32::MAX)) as u32
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

/// One controller value taking effect on one part at one moment.
///
/// Controllers are what carry a performance: the notes say which keys were
/// pressed, and these say how. They are kept as a timeline rather than a
/// final value because they change during a piece, and often during a note.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ControlPoint {
    pub(crate) at_units: u32,
    pub(crate) part: u8,
    pub(crate) value: f32,
}

/// Every controller stream this decoder understands, per part, in time order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Controllers {
    /// Part volume, 8.8 fixed, where 1.0 is unity.
    pub(crate) volume: Vec<ControlPoint>,
    /// Stereo position, 0 hard left to 127 hard right, 64 centre.
    pub(crate) pan: Vec<ControlPoint>,
    /// Modulation depth, 8.8 fixed. Drives vibrato.
    pub(crate) modulation: Vec<ControlPoint>,
    /// Pitch bend, in semitones.
    pub(crate) pitch_bend: Vec<ControlPoint>,
    /// Channel pressure, 0 to 127.
    pub(crate) pressure: Vec<ControlPoint>,
    /// Damper pedal, non-zero for down. Holds a note past its written end.
    pub(crate) sustain: Vec<ControlPoint>,
}

impl Controllers {
    /// The value in force for a part at a moment, or `default` if the part
    /// has said nothing yet.
    fn value_at(points: &[ControlPoint], part: u8, at_units: u32, default: f32) -> f32 {
        points
            .iter()
            .filter(|point| point.part == part && point.at_units <= at_units)
            .last()
            .map_or(default, |point| point.value)
    }
}

/// A decoded tune: its notes, how long it runs, the General MIDI program each
/// part asked for through its note request, and the controllers that shape
/// the performance.
#[derive(Clone, Debug, Default)]
pub(crate) struct DecodedTune {
    pub(crate) notes: Vec<TuneNote>,
    pub(crate) duration_units: u32,
    pub(crate) programs: Vec<(u8, u8)>,
    pub(crate) controllers: Controllers,
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

/// File one controller event onto the right timeline.
///
/// The readings here follow delvmod's `delv/sound.py`, which is the only
/// other decoder of this format. Where it calls something an approximation,
/// so is this.
fn record_controller(
    controllers: &mut Controllers,
    at_units: u32,
    part: u8,
    controller: u32,
    value: u32,
) {
    // Volume and modulation arrive as 8.8 fixed point in the value word.
    let fixed_8_8 = |raw: u32| (raw >> 8) as f32 + ((raw & 0xFF) as f32 / 256.0);

    let point = |value: f32| ControlPoint {
        at_units,
        part,
        value,
    };

    match controller {
        CONTROLLER_VOLUME | CONTROLLER_PART_VOLUME => {
            controllers.volume.push(point(fixed_8_8(value)));
        }
        CONTROLLER_MODULATION => {
            controllers.modulation.push(point(fixed_8_8(value)));
        }
        CONTROLLER_PAN => {
            // QTMA carries pan as 256 (hard left) to 512 (hard right);
            // delvmod scales it to the MIDI 0..127 range and clamps.
            let scaled = (value as i64 - 256) / 2;
            controllers
                .pan
                .push(point(scaled.clamp(0, 127) as f32));
        }
        CONTROLLER_PITCH_BEND => {
            // QTMA stores bend as two seven-bit fields of fractional
            // semitones, and the mapping is not documented. delvmod's
            // reading, arrived at against known tracks and labelled a hack
            // there, is followed exactly: treat the low seven bits as the
            // magnitude, and take a non-zero second field to mean the bend
            // is negative. Cythera's bends are small fractions of a
            // semitone, so the approximation is inaudible either way.
            let magnitude = (value & 0x7F) as f32;
            let sign_field = (value >> 8) & 0x7F;
            let midi_units = if sign_field != 0 {
                (magnitude - 127.0) * 7.4
            } else {
                magnitude
            };
            // A MIDI bend of 8,192 units is conventionally two semitones.
            controllers
                .pitch_bend
                .push(point(midi_units / 8192.0 * 2.0));
        }
        CONTROLLER_AFTERTOUCH => {
            controllers.pressure.push(point((value & 0x7F) as f32));
        }
        CONTROLLER_SUSTAIN => {
            controllers
                .sustain
                .push(point(if value > 0 { 1.0 } else { 0.0 }));
        }
        // Reverb and the rest are decoded by delvmod and not modelled here;
        // this synthesiser has no effects to send them to.
        _ => {}
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
                let part = extract(word, EVENT_PART_POS, EVENT_PART_WIDTH) as u8;
                let controller = extract(word, CONTROL_CONTROLLER_POS, CONTROL_CONTROLLER_WIDTH);
                let value = extract(word, CONTROL_VALUE_POS, CONTROL_VALUE_WIDTH);
                record_controller(&mut tune.controllers, now_units, part, controller, value);
            }
            X_CONTROL_EVENT_TYPE if length >= 2 => {
                let second = words[index + 1];
                let part = extract(word, X_EVENT_PART_POS, X_EVENT_PART_WIDTH) as u8;
                let controller =
                    extract(second, X_CONTROL_CONTROLLER_POS, X_CONTROL_CONTROLLER_WIDTH);
                let value = extract(second, X_CONTROL_VALUE_POS, X_CONTROL_VALUE_WIDTH);
                record_controller(&mut tune.controllers, now_units, part, controller, value);
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
    // Left and right are kept apart from here so that pan is real stereo
    // rather than the same signal twice.
    let mut buffer = vec![(0.0f32, 0.0f32); frames];

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
        let mut sustain_frames = ((duration_ms as f32 / 1000.0) * rate) as usize;
        // A sung note starts far more gently than a struck one.
        let attack_seconds = if voicing == Voicing::Vocal { 0.045 } else { 0.008 };
        let attack = ((attack_seconds * rate) as usize).max(1);

        // Controllers in force where this note begins. Volume, pan, pressure
        // and the damper move slowly enough that reading them once at the
        // start is faithful; bend and modulation are read per frame below,
        // because their whole purpose is to move during a note.
        let controls = &tune.controllers;
        let part_volume =
            Controllers::value_at(&controls.volume, note.part, note.start_units, 1.0);
        let pan = Controllers::value_at(&controls.pan, note.part, note.start_units, 64.0);
        let pressure =
            Controllers::value_at(&controls.pressure, note.part, note.start_units, 0.0);
        let damped =
            Controllers::value_at(&controls.sustain, note.part, note.start_units, 0.0) > 0.5;
        if damped {
            // The pedal holds a note until it is lifted; without tracking the
            // lift, hold it through the tail that is already rendered.
            sustain_frames = length;
        }

        // Equal-power panning, so moving a part across the image does not
        // change how loud it is.
        let pan_angle = (pan / 127.0).clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
        let (left_gain, right_gain) = (pan_angle.cos(), pan_angle.sin());

        let master = (volume_fixed as f32 / 65536.0).clamp(0.0, 1.0);
        // Aftertouch leans on the note rather than replacing its velocity.
        let pressure_gain = 1.0 + (pressure / 127.0) * 0.35;
        let gain = (f32::from(note.volume) / 127.0)
            * (0.4 + 0.6 * part_volume.clamp(0.0, 2.0))
            * pressure_gain
            * 0.28
            * master;
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
            let seconds = frame as f32 / rate;
            let now_units = note
                .start_units
                .saturating_add(ms_to_units((seconds * 1000.0) as u32, time_scale));

            // Vibrato has two sources: a sung part carries its own, and the
            // modulation wheel asks for it on any part. They add.
            let modulation =
                Controllers::value_at(&controls.modulation, note.part, now_units, 0.0);
            let own_depth = if voicing == Voicing::Vocal {
                0.006 * (seconds / 0.25).min(1.0)
            } else {
                0.0
            };
            let depth = own_depth + (modulation.clamp(0.0, 1.0) * 0.02);
            let vibrato = if depth > 0.0 {
                let phase = (seconds * VIBRATO_HZ * SINE_TABLE_LEN as f32) as usize;
                1.0 + depth * table[phase & (SINE_TABLE_LEN - 1)]
            } else {
                1.0
            };

            // Pitch bend, in semitones, applied as a frequency ratio.
            let bend =
                Controllers::value_at(&controls.pitch_bend, note.part, now_units, 0.0);
            let bend_ratio = if bend == 0.0 {
                1.0
            } else {
                2.0f32.powf(bend / 12.0)
            };
            let vibrato = vibrato * bend_ratio;
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

            let value = sample * envelope * gain;
            buffer[start + frame].0 += value * left_gain;
            buffer[start + frame].1 += value * right_gain;
        }
    }

    // Normalise to just under full scale so a dense tune does not clip and a
    // sparse one is still audible. Both channels share one scale, or panning
    // would shift the balance.
    let peak = buffer
        .iter()
        .fold(0.0f32, |acc, (l, r)| acc.max(l.abs()).max(r.abs()));
    let scale = if peak > 0.0 { 0.89 / peak } else { 0.0 };
    let to_byte = |sample: f32| {
        let scaled = (sample * scale).clamp(-1.0, 1.0);
        (scaled * 127.0 + 128.0).round().clamp(0.0, 255.0) as u8
    };

    buffer
        .into_iter()
        .map(|(left, right)| StereoSample {
            left: to_byte(left),
            right: to_byte(right),
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

    fn control(part: u32, controller: u32, value: u32) -> u32 {
        (CONTROL_EVENT_TYPE << EVENT_TYPE_POS)
            | (part << EVENT_PART_POS)
            | (controller << CONTROL_CONTROLLER_POS)
            | value
    }

    #[test]
    fn pan_places_a_part_in_the_stereo_image() {
        // QTMA pan runs 256 hard left to 512 hard right. Without this the
        // renderer emitted the same signal to both channels and every part
        // sat in the middle.
        let hard_left = [
            control(0, CONTROLLER_PAN, 256),
            note(0, 60, 127, 500),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&hard_left);
        assert_eq!(tune.controllers.pan.len(), 1);
        assert_eq!(tune.controllers.pan[0].value, 0.0);
        let samples = render_tune(&tune, 0x0001_0000, 600);
        let left: i32 = samples.iter().map(|s| (s.left as i32 - 128).abs()).sum();
        let right: i32 = samples.iter().map(|s| (s.right as i32 - 128).abs()).sum();
        assert!(left > right * 4, "hard left: left={left} right={right}");
    }

    #[test]
    fn pan_is_symmetric() {
        let right_side = [
            control(0, CONTROLLER_PAN, 512),
            note(0, 60, 127, 500),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&right_side);
        assert_eq!(tune.controllers.pan[0].value, 127.0);
        let samples = render_tune(&tune, 0x0001_0000, 600);
        let left: i32 = samples.iter().map(|s| (s.left as i32 - 128).abs()).sum();
        let right: i32 = samples.iter().map(|s| (s.right as i32 - 128).abs()).sum();
        assert!(right > left * 4, "hard right: left={left} right={right}");
    }

    #[test]
    fn volume_and_modulation_are_eight_eight_fixed_point() {
        // 0x0180 is 1.5, not 384.
        let words = [
            control(0, CONTROLLER_VOLUME, 0x0180),
            control(0, CONTROLLER_MODULATION, 0x0080),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&words);
        assert_eq!(tune.controllers.volume[0].value, 1.5);
        assert_eq!(tune.controllers.modulation[0].value, 0.5);
    }

    #[test]
    fn a_controller_timeline_keeps_every_change_in_order() {
        // Controllers move during a piece; keeping only the last value would
        // apply the end of a fade to its beginning.
        let words = [
            control(0, CONTROLLER_VOLUME, 0x0100),
            rest(600),
            control(0, CONTROLLER_VOLUME, 0x0080),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&words);
        assert_eq!(tune.controllers.volume.len(), 2);
        assert_eq!(tune.controllers.volume[0].at_units, 0);
        assert_eq!(tune.controllers.volume[1].at_units, 600);
        assert_eq!(
            Controllers::value_at(&tune.controllers.volume, 0, 599, 1.0),
            1.0
        );
        assert_eq!(
            Controllers::value_at(&tune.controllers.volume, 0, 600, 1.0),
            0.5
        );
    }

    #[test]
    fn aftertouch_and_the_damper_are_decoded() {
        let words = [
            control(0, CONTROLLER_AFTERTOUCH, 64),
            control(0, CONTROLLER_SUSTAIN, 1),
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&words);
        assert_eq!(tune.controllers.pressure[0].value, 64.0);
        assert_eq!(tune.controllers.sustain[0].value, 1.0);
    }

    #[test]
    fn pitch_bend_follows_delvmods_reading_and_stays_small() {
        // Cythera's bends are fractions of a semitone; a reading that made
        // them whole semitones would be audibly out of tune.
        let words = [control(0, CONTROLLER_PITCH_BEND, 40), END_MARKER_VALUE];
        let tune = decode_tune(&words);
        let bend = tune.controllers.pitch_bend[0].value;
        assert!(bend.abs() < 0.5, "bend of {bend} semitones is too large");
    }

    #[test]
    fn an_extended_control_event_records_the_same_way() {
        let words = [
            (2u32 << EVENT_LENGTH_POS)
                | (X_CONTROL_EVENT_TYPE << X_EVENT_TYPE_POS)
                | (3 << X_EVENT_PART_POS),
            // Length field 2 in the top bits, as delvmod's validity check
            // requires, then the controller and the value.
            (2u32 << EVENT_LENGTH_POS)
                | (CONTROLLER_PAN << X_CONTROL_CONTROLLER_POS)
                | 512,
            END_MARKER_VALUE,
        ];
        let tune = decode_tune(&words);
        let point = tune
            .controllers
            .pan
            .iter()
            .find(|point| point.part == 3)
            .expect("the extended form records against its own part");
        assert_eq!(point.value, 127.0, "512 is hard right in either form");
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

/// Standard MIDI File support, so a tune can be played from a MIDI file in
/// place of the QTMA stream the game queued.
///
/// This exists because Cythera's music is more widely known from the
/// community's MIDI transcriptions than from QuickTime's rendering of the
/// original; substituting at this level keeps the game in charge of *which*
/// music plays and when, and replaces only the notes.
pub(crate) mod midi {
    use super::{DecodedTune, TuneNote};

    /// A MIDI file's times are in ticks against a tempo map. Converting to
    /// milliseconds and reporting a scale of 1000 lets the rest of the player
    /// treat a MIDI tune exactly like a QTMA one.
    pub(crate) const MIDI_TIME_SCALE: u32 = 1000;

    /// The largest file this will parse, as a guard on a caller-supplied path.
    pub(crate) const MAX_MIDI_BYTES: usize = 8 * 1024 * 1024;

    struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        fn u8(&mut self) -> Option<u8> {
            let byte = *self.data.get(self.pos)?;
            self.pos += 1;
            Some(byte)
        }

        fn u16(&mut self) -> Option<u16> {
            Some(u16::from_be_bytes([self.u8()?, self.u8()?]))
        }

        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_be_bytes([
                self.u8()?,
                self.u8()?,
                self.u8()?,
                self.u8()?,
            ]))
        }

        /// A MIDI variable-length quantity: seven bits per byte, high bit set
        /// on every byte but the last.
        fn vlq(&mut self) -> Option<u32> {
            let mut value: u32 = 0;
            for _ in 0..4 {
                let byte = self.u8()?;
                value = (value << 7) | u32::from(byte & 0x7F);
                if byte & 0x80 == 0 {
                    return Some(value);
                }
            }
            None
        }
    }

    /// One note as it appears in the file, before tempo is applied.
    struct TickNote {
        start_tick: u64,
        end_tick: u64,
        channel: u8,
        pitch: u8,
        velocity: u8,
    }

    /// Parse a Standard MIDI File into the same shape a QTMA stream decodes
    /// to, with times in milliseconds.
    ///
    /// Format 0 and format 1 are both handled by merging every track onto one
    /// timeline; a MIDI channel becomes a part. SMPTE division is rejected
    /// rather than guessed at.
    pub(crate) fn decode_midi(bytes: &[u8]) -> Option<DecodedTune> {
        if bytes.len() > MAX_MIDI_BYTES {
            return None;
        }
        let mut reader = Reader { data: bytes, pos: 0 };
        if bytes.get(0..4)? != b"MThd" {
            return None;
        }
        reader.pos = 4;
        let header_length = reader.u32()?;
        if header_length < 6 {
            return None;
        }
        let _format = reader.u16()?;
        let track_count = reader.u16()?;
        let division = reader.u16()?;
        // A negative division is SMPTE timecode, a different clock entirely.
        if division & 0x8000 != 0 || division == 0 {
            return None;
        }
        let ticks_per_quarter = u64::from(division);
        reader.pos = 8 + header_length as usize;

        let mut notes: Vec<TickNote> = Vec::new();
        let mut programs: Vec<(u8, u8)> = Vec::new();
        let mut channel_volume: Vec<(u8, u8)> = Vec::new();
        // (tick, microseconds per quarter note), always starting at 120 bpm.
        let mut tempo_map: Vec<(u64, u32)> = vec![(0, 500_000)];

        for _ in 0..track_count {
            if reader.pos + 8 > bytes.len() {
                break;
            }
            if bytes.get(reader.pos..reader.pos + 4)? != b"MTrk" {
                break;
            }
            reader.pos += 4;
            let track_length = reader.u32()? as usize;
            let track_end = (reader.pos + track_length).min(bytes.len());

            let mut tick: u64 = 0;
            let mut running_status: Option<u8> = None;
            let mut active: Vec<(u8, u8, u64, u8)> = Vec::new();

            while reader.pos < track_end {
                let delta = reader.vlq()?;
                tick += u64::from(delta);
                let mut status = *bytes.get(reader.pos)?;
                if status & 0x80 != 0 {
                    reader.pos += 1;
                    running_status = Some(status);
                } else {
                    status = running_status?;
                }

                match status {
                    0xFF => {
                        let meta = reader.u8()?;
                        let length = reader.vlq()? as usize;
                        let payload = bytes.get(reader.pos..reader.pos + length)?;
                        reader.pos += length;
                        // Set Tempo.
                        if meta == 0x51 && payload.len() == 3 {
                            let micros = (u32::from(payload[0]) << 16)
                                | (u32::from(payload[1]) << 8)
                                | u32::from(payload[2]);
                            if micros > 0 {
                                tempo_map.push((tick, micros));
                            }
                        }
                        // End of Track.
                        if meta == 0x2F {
                            break;
                        }
                    }
                    // System exclusive: skip its declared length.
                    0xF0 | 0xF7 => {
                        let length = reader.vlq()? as usize;
                        reader.pos = (reader.pos + length).min(track_end);
                    }
                    _ => {
                        let kind = status & 0xF0;
                        let channel = status & 0x0F;
                        match kind {
                            // Program Change and Channel Pressure take one byte.
                            0xC0 | 0xD0 => {
                                let value = reader.u8()?;
                                if kind == 0xC0 {
                                    programs.retain(|(c, _)| *c != channel);
                                    programs.push((channel, value & 0x7F));
                                }
                            }
                            _ => {
                                let first = reader.u8()?;
                                let second = reader.u8()?;
                                match kind {
                                    0x90 if second > 0 => {
                                        active.push((channel, first, tick, second));
                                    }
                                    0x80 | 0x90 => {
                                        if let Some(index) = active.iter().rposition(|entry| {
                                            entry.0 == channel && entry.1 == first
                                        }) {
                                            let (_, pitch, start, velocity) = active.remove(index);
                                            notes.push(TickNote {
                                                start_tick: start,
                                                end_tick: tick,
                                                channel,
                                                pitch,
                                                velocity,
                                            });
                                        }
                                    }
                                    // Channel Volume.
                                    0xB0 if first == 7 => {
                                        channel_volume.retain(|(c, _)| *c != channel);
                                        channel_volume.push((channel, second & 0x7F));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            reader.pos = track_end;
        }

        if notes.is_empty() {
            return None;
        }

        tempo_map.sort_by_key(|(tick, _)| *tick);
        let to_ms = |tick: u64| -> u32 {
            let mut millis = 0f64;
            let mut last_tick = 0u64;
            let mut last_tempo = tempo_map[0].1;
            for (at, tempo) in tempo_map.iter().skip(1) {
                if *at >= tick {
                    break;
                }
                millis += (at - last_tick) as f64 / ticks_per_quarter as f64
                    * (f64::from(last_tempo) / 1000.0);
                last_tick = *at;
                last_tempo = *tempo;
            }
            millis += (tick - last_tick) as f64 / ticks_per_quarter as f64
                * (f64::from(last_tempo) / 1000.0);
            millis.max(0.0).min(f64::from(u32::MAX)) as u32
        };

        let mut tune = DecodedTune::default();
        for note in &notes {
            let start = to_ms(note.start_tick);
            let end = to_ms(note.end_tick.max(note.start_tick));
            tune.notes.push(TuneNote {
                start_units: start,
                // A zero-length note would be inaudible; give it the shortest
                // duration the renderer can still shape an envelope over.
                duration_units: end.saturating_sub(start).max(20),
                part: note.channel,
                pitch: note.pitch,
                volume: note.velocity,
            });
        }
        tune.programs = programs;
        // A MIDI channel volume is 0..127 where the QTMA one is a gain.
        tune.controllers.volume = channel_volume
            .into_iter()
            .map(|(channel, volume)| super::ControlPoint {
                at_units: 0,
                part: channel,
                value: f32::from(volume) / 100.0,
            })
            .collect();
        tune.duration_units = tune
            .notes
            .iter()
            .map(|note| note.start_units.saturating_add(note.duration_units))
            .max()
            .unwrap_or(0);
        Some(tune)
    }
}

#[cfg(test)]
mod midi_tests {
    use super::midi::*;
    use super::*;

    /// Build a one-track Standard MIDI File: a middle C held for one beat.
    fn one_note_file(division: u16, tempo_micros: Option<u32>) -> Vec<u8> {
        let mut track: Vec<u8> = Vec::new();
        if let Some(micros) = tempo_micros {
            track.extend_from_slice(&[0x00, 0xFF, 0x51, 0x03]);
            track.extend_from_slice(&micros.to_be_bytes()[1..]);
        }
        // Program change on channel 0 to General MIDI 54.
        track.extend_from_slice(&[0x00, 0xC0, 54]);
        // Note on, then note off a whole division later.
        track.extend_from_slice(&[0x00, 0x90, 60, 100]);
        track.push(0x81);
        track.push(0x48); // delta of 200 as a variable-length quantity
        track.extend_from_slice(&[0x80, 60, 0]);
        track.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);

        let mut file: Vec<u8> = Vec::new();
        file.extend_from_slice(b"MThd");
        file.extend_from_slice(&6u32.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes());
        file.extend_from_slice(&1u16.to_be_bytes());
        file.extend_from_slice(&division.to_be_bytes());
        file.extend_from_slice(b"MTrk");
        file.extend_from_slice(&(track.len() as u32).to_be_bytes());
        file.extend_from_slice(&track);
        file
    }

    #[test]
    fn a_midi_file_decodes_to_notes_in_milliseconds() {
        // 200 ticks at 100 ticks per quarter is two quarters; at the default
        // 500,000 microseconds per quarter that is one second.
        let tune = decode_midi(&one_note_file(100, None)).expect("parses");
        assert_eq!(tune.notes.len(), 1);
        assert_eq!(tune.notes[0].pitch, 60);
        assert_eq!(tune.notes[0].start_units, 0);
        let ms = tune.notes[0].duration_units;
        assert!((990..=1010).contains(&ms), "expected about 1000 ms, got {ms}");
        assert_eq!(tune.program_for(0), 54, "the program change is honoured");
    }

    #[test]
    fn a_tempo_change_is_applied() {
        // Twice as fast: 250,000 microseconds per quarter halves the duration.
        let tune = decode_midi(&one_note_file(100, Some(250_000))).expect("parses");
        let ms = tune.notes[0].duration_units;
        assert!((490..=510).contains(&ms), "expected about 500 ms, got {ms}");
    }

    #[test]
    fn midi_times_are_reported_against_a_scale_of_one_thousand() {
        // The rest of the player works in time-scale units, so a MIDI tune
        // declares the scale that makes its milliseconds come out right.
        assert_eq!(units_to_ms(1000, MIDI_TIME_SCALE), 1000);
    }

    #[test]
    fn a_file_that_is_not_midi_is_refused_rather_than_guessed_at() {
        assert!(decode_midi(b"not a midi file at all").is_none());
        assert!(decode_midi(&[]).is_none());
    }

    #[test]
    fn smpte_division_is_refused_rather_than_misread() {
        // A negative division is SMPTE timecode, a different clock; reading it
        // as ticks-per-quarter would give wildly wrong durations.
        let mut file = one_note_file(100, None);
        file[12] = 0xE7; // division high byte with the sign bit set
        file[13] = 0x28;
        assert!(decode_midi(&file).is_none());
    }

    #[test]
    fn a_truncated_file_does_not_panic() {
        let full = one_note_file(100, None);
        for cut in 0..full.len() {
            let _ = decode_midi(&full[..cut]);
        }
    }

    #[test]
    fn a_substituted_tune_renders_like_any_other() {
        let tune = decode_midi(&one_note_file(100, None)).expect("parses");
        let samples = render_tune(&tune, 0x0001_0000, MIDI_TIME_SCALE);
        assert!(samples.iter().any(|s| s.left != 0x80));
    }
}

/// Playing a recording in place of a tune.
///
/// A synthesiser cannot sound like a performance, and for music that someone
/// has actually recorded there is no reason to make it try. A tune with a
/// recording installed plays the recording; the application still chooses
/// which tune, when it starts and stops, and how loud it is.
///
/// WAV rather than anything else because it needs no decoder: the bytes are
/// already samples. Anything else converts first -- on a Mac, `afconvert -f
/// WAVE -d LEI16 in.m4a out.wav`.
pub(crate) mod wav {
    use crate::sound::StereoSample;

    /// The largest recording this will load. A minute of 44.1 kHz stereo is
    /// about ten megabytes, and the longest of Cythera's tunes is three and a
    /// half minutes.
    pub(crate) const MAX_WAV_BYTES: usize = 64 * 1024 * 1024;

    pub(crate) struct Recording {
        pub(crate) samples: Vec<StereoSample>,
        pub(crate) sample_rate: u32,
    }

    impl Recording {
        pub(crate) fn duration_ms(&self) -> u32 {
            if self.sample_rate == 0 {
                return 0;
            }
            ((self.samples.len() as u64 * 1000) / u64::from(self.sample_rate))
                .min(u64::from(u32::MAX)) as u32
        }
    }

    fn u16le(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_le_bytes([
            *bytes.get(at)?,
            *bytes.get(at + 1)?,
        ]))
    }

    fn u32le(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes([
            *bytes.get(at)?,
            *bytes.get(at + 1)?,
            *bytes.get(at + 2)?,
            *bytes.get(at + 3)?,
        ]))
    }

    /// Read a RIFF/WAVE file of uncompressed PCM.
    ///
    /// 8- and 16-bit are accepted, mono or stereo. Everything else is
    /// refused rather than misread: a compressed WAV read as PCM is loud
    /// noise, which is a worse failure than silence.
    ///
    /// The Sound Manager channel this ends up on carries unsigned 8-bit
    /// samples, so a 16-bit recording is reduced to that. The rate is kept
    /// and handed to the mixer, which resamples.
    pub(crate) fn decode_wav(bytes: &[u8]) -> Option<Recording> {
        if bytes.len() > MAX_WAV_BYTES || bytes.len() < 12 {
            return None;
        }
        if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
            return None;
        }

        let mut format: Option<(u16, u16, u32, u16, usize, usize)> = None;
        let mut data: Option<(usize, usize)> = None;
        let mut at = 12usize;
        // Walk the chunks rather than assuming fmt is first and data second;
        // plenty of writers put LIST or fact between them.
        while at + 8 <= bytes.len() {
            let id = bytes.get(at..at + 4)?;
            let size = u32le(bytes, at + 4)? as usize;
            let body = at + 8;
            if id == b"fmt " && size >= 16 {
                format = Some((
                    u16le(bytes, body)?,      // format tag
                    u16le(bytes, body + 2)?,  // channels
                    u32le(bytes, body + 4)?,  // sample rate
                    u16le(bytes, body + 14)?, // bits per sample
                    body,
                    size,
                ));
            } else if id == b"data" {
                data = Some((body, size.min(bytes.len().saturating_sub(body))));
            }
            // Chunks are padded to an even length.
            at = body + size + (size & 1);
        }

        let (tag, channels, sample_rate, bits, fmt_at, fmt_size) = format?;
        let (start, length) = data?;

        // 1 is WAVE_FORMAT_PCM. 0xFFFE is WAVE_FORMAT_EXTENSIBLE, which is
        // what macOS's afconvert writes and what most modern tools emit: the
        // real format is a GUID at offset 24 of the chunk, whose first two
        // bytes are the tag and whose remaining fourteen are a fixed suffix.
        // Checking that suffix is what keeps this from reading some other
        // extensible format as PCM and playing it as noise.
        const PCM_GUID_SUFFIX: [u8; 14] = [
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ];
        let effective_tag = if tag == 0xFFFE {
            if fmt_size < 40 {
                return None;
            }
            if bytes.get(fmt_at + 26..fmt_at + 40)? != PCM_GUID_SUFFIX {
                return None;
            }
            u16le(bytes, fmt_at + 24)?
        } else {
            tag
        };
        if effective_tag != 1 || sample_rate == 0 || channels == 0 || channels > 2 {
            return None;
        }

        let frames;
        let mut samples = Vec::new();
        match bits {
            8 => {
                // 8-bit WAV is already unsigned, the same convention the
                // Sound Manager uses.
                frames = length / usize::from(channels);
                for frame in 0..frames {
                    let base = start + frame * usize::from(channels);
                    let left = *bytes.get(base)?;
                    let right = if channels == 2 { *bytes.get(base + 1)? } else { left };
                    samples.push(StereoSample { left, right });
                }
            }
            16 => {
                let stride = 2 * usize::from(channels);
                frames = length / stride;
                for frame in 0..frames {
                    let base = start + frame * stride;
                    let left = u16le(bytes, base)? as i16;
                    let right = if channels == 2 {
                        u16le(bytes, base + 2)? as i16
                    } else {
                        left
                    };
                    // Signed 16-bit to unsigned 8-bit.
                    let to_byte = |value: i16| ((value >> 8) as i32 + 128).clamp(0, 255) as u8;
                    samples.push(StereoSample {
                        left: to_byte(left),
                        right: to_byte(right),
                    });
                }
            }
            _ => return None,
        }

        if samples.is_empty() {
            return None;
        }
        Some(Recording { samples, sample_rate })
    }
}

#[cfg(test)]
mod wav_tests {
    use super::wav::*;

    /// A RIFF/WAVE file of uncompressed PCM.
    fn build(channels: u16, bits: u16, rate: u32, body: &[u8], tag: u16) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // size, unread
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        // A chunk between fmt and data, which plenty of writers emit.
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(b"INFO");
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn sixteen_bit_stereo_becomes_unsigned_eight_bit() {
        // Three stereo frames, both channels the same in each: full
        // positive, full negative, silence. Interleaved, so six values.
        let body: Vec<u8> = [0x7FFFi16, 0x7FFF, -0x8000, -0x8000, 0, 0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let r = decode_wav(&build(2, 16, 44100, &body, 1)).expect("parses");
        assert_eq!(r.sample_rate, 44100);
        assert_eq!(r.samples.len(), 3);
        assert_eq!(r.samples[0].left, 255, "full positive is full scale");
        assert_eq!(r.samples[1].left, 0, "full negative is zero");
        assert_eq!(r.samples[2].left, 128, "silence is the midpoint");
    }

    #[test]
    fn mono_is_carried_to_both_channels() {
        let body: Vec<u8> = [1000i16, -1000].iter().flat_map(|v| v.to_le_bytes()).collect();
        let r = decode_wav(&build(1, 16, 22050, &body, 1)).expect("parses");
        assert_eq!(r.samples.len(), 2);
        for s in &r.samples {
            assert_eq!(s.left, s.right, "a mono recording sits in the middle");
        }
    }

    #[test]
    fn eight_bit_is_already_unsigned() {
        let r = decode_wav(&build(2, 8, 22050, &[0, 255, 128, 128], 1)).expect("parses");
        assert_eq!(r.samples.len(), 2);
        assert_eq!((r.samples[0].left, r.samples[0].right), (0, 255));
        assert_eq!((r.samples[1].left, r.samples[1].right), (128, 128));
    }

    #[test]
    fn a_chunk_between_fmt_and_data_is_stepped_over() {
        // The builder always writes a LIST chunk between them, so every test
        // above covers this; this one says so out loud.
        let body: Vec<u8> = [0i16; 4].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert!(decode_wav(&build(2, 16, 44100, &body, 1)).is_some());
    }

    /// WAVE_FORMAT_EXTENSIBLE, as afconvert and most modern tools write it:
    /// a 40-byte fmt chunk whose real format is a GUID at offset 24.
    fn build_extensible(channels: u16, bits: u16, rate: u32, body: &[u8], sub: u16,
                        suffix: [u8; 14]) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt.extend_from_slice(&22u16.to_le_bytes());   // cbSize
        fmt.extend_from_slice(&bits.to_le_bytes());    // valid bits
        fmt.extend_from_slice(&3u32.to_le_bytes());    // channel mask
        fmt.extend_from_slice(&sub.to_le_bytes());
        fmt.extend_from_slice(&suffix);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    const PCM_SUFFIX: [u8; 14] = [
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ];

    #[test]
    fn extensible_pcm_is_accepted_because_afconvert_writes_it() {
        // Every recording converted on a Mac arrives in this form; refusing
        // it meant no recording could be played at all.
        let body: Vec<u8> = [0x7FFFi16, 0x7FFF].iter().flat_map(|v| v.to_le_bytes()).collect();
        let r = decode_wav(&build_extensible(2, 16, 44100, &body, 1, PCM_SUFFIX))
            .expect("extensible PCM parses");
        assert_eq!(r.sample_rate, 44100);
        assert_eq!(r.samples[0].left, 255);
    }

    #[test]
    fn an_extensible_wav_that_is_not_pcm_is_still_refused() {
        let body: Vec<u8> = vec![0x40; 32];
        // Sub-format 3 is IEEE float, which these samples are not.
        assert!(decode_wav(&build_extensible(2, 16, 44100, &body, 3, PCM_SUFFIX)).is_none());
        // A PCM tag with a foreign GUID suffix is some other format entirely.
        let mut wrong = PCM_SUFFIX; wrong[0] = 0x99;
        assert!(decode_wav(&build_extensible(2, 16, 44100, &body, 1, wrong)).is_none());
    }

    #[test]
    fn a_compressed_wav_is_refused_rather_than_played_as_noise() {
        // Format tag 0x11 is IMA ADPCM. Read as PCM it is loud noise, which
        // is a worse failure than declining it.
        let body: Vec<u8> = vec![0x40; 64];
        assert!(decode_wav(&build(2, 4, 44100, &body, 0x11)).is_none());
        // 24-bit PCM is honest PCM this decoder does not handle.
        assert!(decode_wav(&build(2, 24, 44100, &body, 1)).is_none());
    }

    #[test]
    fn rubbish_is_refused_and_nothing_panics() {
        assert!(decode_wav(b"").is_none());
        assert!(decode_wav(b"RIFF").is_none());
        assert!(decode_wav(b"not a wav file at all").is_none());
        let full = build(2, 16, 44100, &[0u8; 32], 1);
        for cut in 0..full.len() {
            let _ = decode_wav(&full[..cut]);
        }
    }

    #[test]
    fn duration_comes_from_the_frame_count_and_the_rate() {
        let body: Vec<u8> = vec![0u8; 44100 * 4]; // one second of 16-bit stereo
        let r = decode_wav(&build(2, 16, 44100, &body, 1)).expect("parses");
        assert_eq!(r.duration_ms(), 1000);
    }
}
