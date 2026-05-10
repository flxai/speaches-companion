use std::time::Duration;

use crate::audio::pcm_bytes_for_duration;

pub const DEFAULT_SPEECH_ANALYSIS_FRAME: Duration = Duration::from_millis(20);
pub const DEFAULT_MIN_SPEECH_DURATION: Duration = Duration::from_millis(120);
pub const DEFAULT_SPEECH_RMS_FLOOR: f64 = 700.0;
pub const DEFAULT_MAX_SPEECH_RMS_FLOOR: f64 = 1_500.0;
pub const DEFAULT_NOISE_FLOOR_MULTIPLIER: f64 = 4.0;
pub const DEFAULT_NOISE_SAMPLE_DURATION: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct RmsFrame {
    pub start: usize,
    pub end: usize,
    pub rms: f64,
}

#[derive(Debug, Clone)]
pub struct SpeechActivityDetector {
    sample_rate: u32,
    frame: Duration,
    threshold: f64,
}

impl SpeechActivityDetector {
    pub fn new(sample_rate: u32) -> Self {
        Self::with_initial_noise(sample_rate, &[])
    }

    pub fn with_initial_noise(sample_rate: u32, initial_pcm: &[u8]) -> Self {
        let frames = rms_frames(sample_rate, initial_pcm, DEFAULT_SPEECH_ANALYSIS_FRAME);
        Self {
            sample_rate,
            frame: DEFAULT_SPEECH_ANALYSIS_FRAME,
            threshold: adaptive_speech_threshold(sample_rate, &frames),
        }
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    pub fn has_speech(&self, pcm: &[u8]) -> bool {
        rms_frames(self.sample_rate, pcm, self.frame)
            .iter()
            .any(|frame| frame.rms >= self.threshold)
    }
}

pub fn rms_frames(sample_rate: u32, pcm: &[u8], frame: Duration) -> Vec<RmsFrame> {
    let frame_bytes = pcm_bytes_for_duration(sample_rate, frame).max(2);
    let mut frames = Vec::new();
    let mut start = 0usize;
    while start + 2 <= pcm.len() {
        let end = (start + frame_bytes).min(pcm.len());
        frames.push(RmsFrame {
            start,
            end,
            rms: pcm_rms_s16le(&pcm[start..end]),
        });
        start = end;
    }
    frames
}

pub fn adaptive_speech_threshold(sample_rate: u32, frames: &[RmsFrame]) -> f64 {
    adaptive_speech_threshold_with(
        sample_rate,
        frames,
        DEFAULT_NOISE_SAMPLE_DURATION,
        DEFAULT_SPEECH_RMS_FLOOR,
        DEFAULT_MAX_SPEECH_RMS_FLOOR,
        DEFAULT_NOISE_FLOOR_MULTIPLIER,
    )
}

pub fn adaptive_speech_threshold_with(
    sample_rate: u32,
    frames: &[RmsFrame],
    noise_duration: Duration,
    floor: f64,
    max_floor: f64,
    noise_multiplier: f64,
) -> f64 {
    let noise_sample_bytes = pcm_bytes_for_duration(sample_rate, noise_duration);
    let noise_frames = frames
        .iter()
        .take_while(|frame| frame.start < noise_sample_bytes)
        .collect::<Vec<_>>();
    if noise_frames.is_empty() {
        return floor;
    }

    let noise_floor =
        noise_frames.iter().map(|frame| frame.rms).sum::<f64>() / noise_frames.len() as f64;
    floor.max(noise_floor * noise_multiplier).min(max_floor)
}

pub fn pcm_rms_s16le(pcm: &[u8]) -> f64 {
    let mut sum = 0f64;
    let mut count = 0usize;
    for sample in pcm_s16le_samples(pcm) {
        let value = f64::from(sample);
        sum += value * value;
        count += 1;
    }

    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

pub fn pcm_s16le_samples(pcm: &[u8]) -> impl Iterator<Item = i16> + '_ {
    pcm.chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_for_duration(sample_rate: u32, duration: Duration, amplitude: i16) -> Vec<u8> {
        let samples = sample_rate as usize * duration.as_millis() as usize / 1_000;
        let mut pcm = Vec::with_capacity(samples * 2);
        for _ in 0..samples {
            pcm.extend_from_slice(&amplitude.to_le_bytes());
        }
        pcm
    }

    #[test]
    fn adaptive_threshold_uses_floor_without_noise_sample() {
        assert_eq!(SpeechActivityDetector::new(16_000).threshold(), 700.0);
    }

    #[test]
    fn adaptive_threshold_rises_with_noise_floor() {
        let pcm = pcm_for_duration(1_000, Duration::from_secs(1), 400);
        let detector = SpeechActivityDetector::with_initial_noise(1_000, &pcm);

        assert_eq!(detector.threshold(), 1_500.0);
        assert!(!detector.has_speech(&pcm));
        assert!(detector.has_speech(&pcm_for_duration(1_000, Duration::from_millis(20), 2_000)));
    }
}
