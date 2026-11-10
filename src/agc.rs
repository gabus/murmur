//! Automatic gain control for quiet microphones.
//!
//! Silero VAD misses the start of quiet speech (peaks around 0.01 lose whole words),
//! and Parakeet is worse on quiet input. This scales the signal so speech
//! peaks land near `TARGET_PEAK`. The gain is capped so background noise isn't
//! blown up, and it adapts quickly downward (loud speech) but slowly upward
//! (quiet room) so it doesn't pump between words.

/// Desired level for speech peaks.
const TARGET_PEAK: f32 = 0.5;
/// Never amplify more than this (+20 dB).
const MAX_GAIN: f32 = 10.0;
/// Peak envelope decay per second (how fast "loud" is forgotten).
const ENVELOPE_DECAY_PER_SEC: f32 = 0.5;
/// Envelope floor: below this, treat as silence (don't chase the noise floor).
const ENVELOPE_FLOOR: f32 = 0.02;

#[derive(Debug, Clone)]
pub struct Agc {
    envelope: f32,
    gain: f32,
    sample_rate: f32,
}

impl Agc {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            envelope: ENVELOPE_FLOOR,
            gain: 1.0,
            sample_rate: sample_rate as f32,
        }
    }

    /// Current gain (for logs/tests).
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Applies gain in place. Call with consecutive chunks of one stream.
    pub fn process(&mut self, samples: &mut [f32]) {
        if samples.is_empty() {
            return;
        }
        let peak = samples.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let secs = samples.len() as f32 / self.sample_rate;
        let decay = ENVELOPE_DECAY_PER_SEC.powf(secs);
        self.envelope = (self.envelope * decay).max(peak).max(ENVELOPE_FLOOR);
        let want = (TARGET_PEAK / self.envelope).clamp(1.0, MAX_GAIN);
        // Drop immediately (avoid clipping), rise slowly (~1 s to reach target).
        let target = if want < self.gain {
            want
        } else {
            self.gain + (want - self.gain) * (secs / 1.0).min(1.0)
        };
        let start = self.gain;
        let n = samples.len() as f32;
        for (i, s) in samples.iter_mut().enumerate() {
            // Linear ramp across the chunk to avoid clicks.
            let g = start + (target - start) * (i as f32 + 1.0) / n;
            *s = (*s * g).clamp(-1.0, 1.0);
        }
        self.gain = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.05).sin()).collect()
    }

    fn run(agc: &mut Agc, amp: f32, secs: f32) -> f32 {
        let mut peak = 0.0f32;
        for _ in 0..(secs / 0.02) as usize {
            let mut c = tone(amp, 320);
            agc.process(&mut c);
            peak = c.iter().fold(0.0, |m, x| m.max(x.abs()));
        }
        peak
    }

    #[test]
    fn quiet_speech_is_raised_towards_target() {
        let mut agc = Agc::new(16_000);
        let peak = run(&mut agc, 0.05, 3.0);
        assert!(peak > 0.4 && peak <= 0.51, "peak {peak}");
    }

    #[test]
    fn gain_is_capped_for_noise() {
        let mut agc = Agc::new(16_000);
        run(&mut agc, 0.005, 5.0);
        assert!(agc.gain() <= MAX_GAIN + 1e-3);
    }

    #[test]
    fn loud_input_is_not_amplified_and_never_clips_badly() {
        let mut agc = Agc::new(16_000);
        let peak = run(&mut agc, 0.9, 2.0);
        assert!((agc.gain() - 1.0).abs() < 1e-6);
        assert!(peak <= 1.0);
    }

    #[test]
    fn sudden_loud_after_quiet_drops_gain_immediately() {
        let mut agc = Agc::new(16_000);
        run(&mut agc, 0.03, 3.0);
        let mut c = tone(0.8, 320);
        agc.process(&mut c);
        // gain fell within one chunk; output is clamped, not wildly over
        assert!(agc.gain() < 1.0 + 1e-3, "gain {}", agc.gain());
    }
}
