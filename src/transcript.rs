/// Which audio source produced a transcript segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    Mic,
    Sys,
}

/// A single transcript event emitted by the active backend.
///
/// For Deepgram, `is_final = false` means an interim result; `true` means
/// committed. For Whisper and Parakeet, all results are final.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub source: AudioSource,
    pub text: String,
    pub is_final: bool,
}
