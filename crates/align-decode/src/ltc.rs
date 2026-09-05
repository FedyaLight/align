//! Conservative Linear Timecode detection from a short mono window.
//! A candidate is accepted only when multiple decoded SMPTE labels advance
//! consecutively; ordinary dialogue/music therefore stays metadata-free.

use align_core::{MediaTime, SourceTimecode};

const MIN_CONSECUTIVE_FRAMES: usize = 8;
const MAX_DECODED_FRAMES: usize = 40;

struct ObservedFrame {
    end_sample: usize,
    hour: i64,
    minute: i64,
    second: i64,
    frame: i64,
    drop_frame: bool,
}

/// Decode the timecode at sample zero. `samples` should contain a few seconds
/// at the source rate so the biphase transitions survive decoding.
pub fn detect(samples: &[f32], sample_rate: f64) -> Option<SourceTimecode> {
    if !sample_rate.is_finite() || samples.len() < (sample_rate.max(1.0) / 4.0) as usize {
        return None;
    }
    let mean = samples.iter().map(|&v| f64::from(v)).sum::<f64>() / samples.len() as f64;
    let mut decoder = ltc::LTCDecoder::with_capacity((sample_rate / 25.0) as f32, 4);
    let mut observed = Vec::new();
    for (index, &sample) in samples.iter().enumerate() {
        let centered = [sample - mean as f32];
        if decoder.write_samples(&centered) {
            for frame in &mut decoder {
                observed.push(ObservedFrame {
                    end_sample: index + 1,
                    hour: i64::from(frame.hour),
                    minute: i64::from(frame.minute),
                    second: i64::from(frame.second),
                    frame: i64::from(frame.frame),
                    drop_frame: frame.drop_frame,
                });
            }
            if observed.len() >= MAX_DECODED_FRAMES {
                break;
            }
        }
    }
    if observed.len() < MIN_CONSECUTIVE_FRAMES {
        return None;
    }

    let mut frame_samples: Vec<usize> = observed
        .windows(2)
        .map(|pair| pair[1].end_sample - pair[0].end_sample)
        .filter(|&length| length > 0)
        .collect();
    frame_samples.sort_unstable();
    let measured_fps = sample_rate / *frame_samples.get(frame_samples.len() / 2)? as f64;
    let drop_frame = observed.iter().filter(|f| f.drop_frame).count() * 2 >= observed.len();
    let frame_duration = nearest_rate(measured_fps, drop_frame)?;

    let decoded: Vec<Option<SourceTimecode>> = observed
        .iter()
        .map(|frame| {
            SourceTimecode::from_components(
                frame.hour,
                frame.minute,
                frame.second,
                frame.frame,
                frame_duration,
                frame.drop_frame,
            )
        })
        .collect();
    let mut run_start = 0;
    let mut run_length = 1;
    let mut accepted = None;
    for index in 1..decoded.len() {
        let consecutive = decoded[index - 1]
            .as_ref()
            .zip(decoded[index].as_ref())
            .is_some_and(|(previous, current)| current.frame_number == previous.frame_number + 1);
        if consecutive {
            run_length += 1;
        } else {
            run_start = index;
            run_length = 1;
        }
        if run_length >= MIN_CONSECUTIVE_FRAMES {
            accepted = Some(run_start);
            break;
        }
    }
    let first_index = accepted?;
    let first = decoded[first_index].as_ref()?;
    let frame = &observed[first_index];
    let frame_start = frame
        .end_sample
        .saturating_sub((sample_rate / measured_fps).round() as usize);
    let preceding_frames = (frame_start as f64 * measured_fps / sample_rate).round() as i64;
    SourceTimecode::from_frame_number(
        first.frame_number.saturating_sub(preceding_frames),
        frame_duration,
        drop_frame,
    )
}

fn nearest_rate(measured: f64, drop_frame: bool) -> Option<MediaTime> {
    let rates = if drop_frame {
        &[(30_000.0 / 1_001.0, MediaTime::new(1_001, 30_000))][..]
    } else {
        &[
            (24_000.0 / 1_001.0, MediaTime::new(1_001, 24_000)),
            (24.0, MediaTime::new(1, 24)),
            (25.0, MediaTime::new(1, 25)),
            (30_000.0 / 1_001.0, MediaTime::new(1_001, 30_000)),
            (30.0, MediaTime::new(1, 30)),
        ][..]
    };
    rates
        .iter()
        .min_by(|(a, _), (b, _)| (measured - a).abs().total_cmp(&(measured - b).abs()))
        .filter(|(rate, _)| (measured - rate).abs() / rate < 0.01)
        .map(|(_, duration)| *duration)
}

#[cfg(test)]
fn set_digit(word: &mut u128, value: u8, positions: &[u8]) {
    for (bit, position) in positions.iter().enumerate() {
        if value & (1 << bit) != 0 {
            *word |= 1_u128 << position;
        }
    }
}

#[cfg(test)]
pub(crate) fn synthetic_ltc(start_frame: u8, frames: usize, fps: u8) -> Vec<f32> {
    let samples_per_bit = 48_000 / usize::from(fps) / 80;
    let half = samples_per_bit / 2;
    let mut level = -0.8;
    let mut samples = vec![level; samples_per_bit * 2];
    for number in start_frame..start_frame + frames as u8 {
        let mut word = 0x3ffd_u128;
        set_digit(&mut word, number % 10, &[79, 78, 77, 76]);
        set_digit(&mut word, number / 10, &[71, 70]);
        set_digit(&mut word, 3, &[63, 62, 61, 60]);
        set_digit(&mut word, 0, &[55, 54, 53]);
        set_digit(&mut word, 2, &[47, 46, 45, 44]);
        set_digit(&mut word, 0, &[39, 38, 37]);
        set_digit(&mut word, 1, &[31, 30, 29, 28]);
        set_digit(&mut word, 0, &[23, 22]);
        for position in (0..80).rev() {
            level = -level;
            samples.extend(std::iter::repeat_n(level, half));
            if word & (1_u128 << position) != 0 {
                level = -level;
            }
            samples.extend(std::iter::repeat_n(level, samples_per_bit - half));
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_and_program_audio_do_not_decode() {
        assert!(detect(&vec![0.0; 48_000], 16_000.0).is_none());
        let tone: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin())
            .collect();
        assert!(detect(&tone, 16_000.0).is_none());
        let mut state = 0xC0FFEE_u64;
        let noise: Vec<f32> = (0..48_000)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 32) as i32 as f32 / i32::MAX as f32
            })
            .collect();
        assert!(detect(&noise, 16_000.0).is_none());
    }

    #[test]
    fn rate_selection_distinguishes_standard_rates() {
        assert_eq!(
            nearest_rate(23.976, false),
            Some(MediaTime::new(1_001, 24_000))
        );
        assert_eq!(nearest_rate(24.0, false), Some(MediaTime::new(1, 24)));
        assert_eq!(nearest_rate(25.0, false), Some(MediaTime::new(1, 25)));
        assert_eq!(
            nearest_rate(29.97, true),
            Some(MediaTime::new(1_001, 30_000))
        );
        assert_eq!(nearest_rate(30.0, false), Some(MediaTime::new(1, 30)));
        assert_eq!(nearest_rate(60.0, false), None);
    }

    #[test]
    fn consecutive_biphase_frames_decode() {
        let samples = synthetic_ltc(0, 16, 25);
        let timecode = detect(&samples, 48_000.0).expect("LTC");
        assert_eq!(timecode.text, "01:02:03:00");
        assert_eq!(timecode.frame_duration, MediaTime::new(1, 25));
    }
}
