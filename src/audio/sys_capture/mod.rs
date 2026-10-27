//! System-audio capture: whatever is coming out of the speakers, as 16kHz-bound
//! mono `f32` samples in a ring buffer.
//!
//! Both platforms present the same thing to the engine — a [`SysAudioTap`] with
//! an authoritative `sample_rate`, a `dropped_samples` counter, and a consumer
//! handing out mono samples — but they get there very differently:
//!
//! | Platform | Mechanism |
//! |---|---|
//! | macOS | A CoreAudio global process tap inside an aggregate device ([`macos`]) |
//! | Windows | WASAPI loopback on the default render device, through cpal ([`windows`]) |
//!
//! Capture runs on an OS audio thread on both, so the callbacks must not
//! allocate, lock, or format strings. Diagnostics go into atomics and the
//! engine reports them.

/// Ring buffer capacity, in `f32` samples.
///
/// 8192 samples is 170ms at 48kHz, which is ample for a 20ms drain tick and
/// leaves room for a scheduling stall.
pub(crate) const RING_CAPACITY: usize = 8192;

/// Upper bound on frames a capture callback will downmix in one call without
/// allocating.
const SCRATCH_FRAMES: usize = RING_CAPACITY;

/// Mix interleaved stereo `raw` down to mono in `scratch`, returning the number
/// of frames written.
///
/// `frames` is the caller's frame count; the result is clamped to both the
/// scratch length and the samples actually present, so the callback neither
/// allocates nor reads past the end of an OS-owned buffer.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn downmix_stereo_into(raw: &[f32], frames: usize, scratch: &mut [f32]) -> usize {
    let frames = frames.min(scratch.len()).min(raw.len() / 2);
    for (f, slot) in scratch[..frames].iter_mut().enumerate() {
        *slot = (raw[f * 2] + raw[f * 2 + 1]) / 2.0;
    }
    frames
}

/// Mix `channels`-wide interleaved `raw` down to mono `f32` in `scratch`,
/// returning the number of frames written.
///
/// Generic over the sample format because a Windows render device's mix format
/// is whatever the user's hardware negotiated — `f32` usually, `i16` on plenty
/// of machines — and because render devices are routinely wider than stereo
/// (5.1, 7.1), unlike the macOS tap which is created mono.
///
/// Writes nothing for a zero-channel buffer rather than dividing by it, and
/// clamps to the scratch length, so the audio callback neither allocates nor
/// faults.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn downmix_into<T>(raw: &[T], channels: usize, scratch: &mut [f32]) -> usize
where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    if channels == 0 {
        return 0;
    }
    let frames = (raw.len() / channels).min(scratch.len());
    let scale = 1.0 / channels as f32;
    for (f, slot) in scratch[..frames].iter_mut().enumerate() {
        let frame = &raw[f * channels..f * channels + channels];
        let sum: f32 = frame
            .iter()
            .map(|&s| <f32 as cpal::FromSample<T>>::from_sample_(s))
            .sum();
        *slot = sum * scale;
    }
    frames
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::SysAudioTap;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::SysAudioTap;

#[cfg(test)]
mod tests {
    use super::*;

    // --- stereo downmix ---

    #[test]
    fn downmix_stereo_averages_each_pair() {
        let raw = [1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        let mut scratch = [0.0f32; 8];
        assert_eq!(downmix_stereo_into(&raw, 3, &mut scratch), 3);
        assert_eq!(&scratch[..3], &[0.5, 0.5, 0.0]);
    }

    #[test]
    fn downmix_stereo_clamps_to_the_scratch_length() {
        let raw = vec![1.0f32; 4096];
        let mut scratch = [0.0f32; 16];
        assert_eq!(downmix_stereo_into(&raw, 2048, &mut scratch), 16);
    }

    #[test]
    fn downmix_stereo_clamps_to_the_available_samples() {
        // A frame count larger than the buffer actually holds must not read
        // past the end of the OS-owned slice.
        let raw = [1.0, 1.0, 2.0, 2.0];
        let mut scratch = [0.0f32; 64];
        assert_eq!(downmix_stereo_into(&raw, 32, &mut scratch), 2);
        assert_eq!(&scratch[..2], &[1.0, 2.0]);
    }

    #[test]
    fn downmix_stereo_of_nothing_writes_nothing() {
        let mut scratch = [9.0f32; 4];
        assert_eq!(downmix_stereo_into(&[], 0, &mut scratch), 0);
        assert_eq!(scratch, [9.0; 4]);
    }

    // --- arbitrary channel counts (Windows render devices) ---

    #[test]
    fn downmix_mono_copies_through() {
        let raw = [1.0, 2.0, 3.0];
        let mut scratch = [0.0f32; 8];
        assert_eq!(downmix_into(&raw, 1, &mut scratch), 3);
        assert_eq!(&scratch[..3], &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn downmix_stereo_via_the_general_path_matches_the_pair_path() {
        let raw = [1.0, 0.0, 0.5, 0.5];
        let mut general = [0.0f32; 4];
        let mut pairwise = [0.0f32; 4];
        let a = downmix_into(&raw, 2, &mut general);
        let b = downmix_stereo_into(&raw, 2, &mut pairwise);
        assert_eq!(a, b);
        assert_eq!(general, pairwise);
    }

    #[test]
    fn downmix_averages_a_surround_frame() {
        // One 6-channel frame; the mean of 0..6 is 2.5.
        let raw = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        let mut scratch = [0.0f32; 4];
        assert_eq!(downmix_into(&raw, 6, &mut scratch), 1);
        assert!((scratch[0] - 2.5).abs() < f32::EPSILON);
    }

    #[test]
    fn downmix_of_zero_channels_is_zero_not_a_divide_by_zero() {
        let mut scratch = [0.0f32; 4];
        assert_eq!(downmix_into(&[1.0, 2.0], 0, &mut scratch), 0);
    }

    #[test]
    fn downmix_ignores_a_partial_trailing_frame() {
        // Five samples across 2 channels is two whole frames plus a stray.
        let raw = [1.0, 1.0, 2.0, 2.0, 3.0];
        let mut scratch = [0.0f32; 8];
        assert_eq!(downmix_into(&raw, 2, &mut scratch), 2);
    }

    #[test]
    fn downmix_clamps_to_the_scratch_length() {
        let raw = vec![1.0f32; 600];
        let mut scratch = [0.0f32; 16];
        assert_eq!(downmix_into(&raw, 6, &mut scratch), 16);
    }

    /// A Windows render device may negotiate an integer mix format, so the same
    /// path has to convert as well as downmix.
    #[test]
    fn downmix_converts_an_integer_mix_format() {
        let raw: [i16; 4] = [i16::MAX, i16::MAX, 0, 0];
        let mut scratch = [0.0f32; 4];
        assert_eq!(downmix_into(&raw, 2, &mut scratch), 2);
        assert!((scratch[0] - 1.0).abs() < 1e-3, "got {}", scratch[0]);
        assert_eq!(scratch[1], 0.0);
    }

    #[test]
    fn downmix_of_an_integer_frame_averages_across_channels() {
        let raw: [i16; 2] = [i16::MAX, 0];
        let mut scratch = [0.0f32; 2];
        assert_eq!(downmix_into(&raw, 2, &mut scratch), 1);
        assert!((scratch[0] - 0.5).abs() < 1e-3, "got {}", scratch[0]);
    }
}
