//! The [`Transcript`] event type and its supporting enums.

use std::time::Duration;

/// Which audio source produced a transcript segment.
///
/// # Example
///
/// ```
/// use splitstream::AudioSource;
///
/// let label = match AudioSource::Mic {
///     AudioSource::Mic => "you",
///     AudioSource::Sys => "them",
///     // `AudioSource` is `#[non_exhaustive]`: a catch-all arm is required.
///     _ => "unknown",
/// };
/// assert_eq!(label, "you");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioSource {
    /// The local microphone (what you said).
    Mic,
    /// System audio captured from the Core Audio process tap (what they said).
    Sys,
}

impl std::fmt::Display for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioSource::Mic => f.write_str("mic"),
            AudioSource::Sys => f.write_str("sys"),
        }
    }
}

/// A single transcript event emitted by the active backend.
///
/// `start` and `end` are offsets from the moment capture began, so segments
/// from the two sources share one timeline and can be interleaved, seeked, or
/// aligned against a recording.
///
/// For Deepgram, `is_final = false` means an interim result that a later event
/// may replace; `true` means committed. Parakeet results are always final.
///
/// This struct is `#[non_exhaustive]`: construct it only from the fields you
/// read, and expect new fields in future releases.
///
/// # Example
///
/// ```
/// # use splitstream::Transcript;
/// fn print(t: &Transcript) {
///     println!(
///         "[{:>6.2}s] {} {}{}",
///         t.start.as_secs_f32(),
///         t.source,
///         t.text,
///         match t.confidence {
///             Some(c) => format!(" (confidence {c:.2})"),
///             None => String::new(),
///         }
///     );
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Transcript {
    /// Which channel produced this segment.
    pub source: AudioSource,
    /// The recognized text, trimmed of leading and trailing whitespace.
    pub text: String,
    /// `false` for a Deepgram interim result, `true` once committed.
    /// Local backends only emit final segments.
    pub is_final: bool,
    /// Offset of the start of this segment from the start of the stream.
    pub start: Duration,
    /// Offset of the end of this segment from the start of the stream.
    /// Always `>= start`.
    pub end: Duration,
    /// Backend-reported confidence in `0.0..=1.0`, when the backend supplies
    /// one. Deepgram does; the local backends do not, and report `None`.
    pub confidence: Option<f32>,
}

impl Transcript {
    /// Build a transcript segment. Internal: the public constructor surface is
    /// deliberately absent so fields can be added without a breaking change.
    // Unused when the crate is built with no backend feature at all.
    #[cfg_attr(
        not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")),
        allow(dead_code)
    )]
    pub(crate) fn new(
        source: AudioSource,
        text: String,
        is_final: bool,
        start: Duration,
        end: Duration,
        confidence: Option<f32>,
    ) -> Self {
        Self {
            source,
            text,
            is_final,
            start,
            end,
            confidence,
        }
    }

    /// How long this segment lasted (`end - start`).
    ///
    /// # Example
    ///
    /// ```
    /// # use splitstream::Transcript;
    /// # fn f(t: &Transcript) {
    /// assert!(t.duration() == t.end - t.start);
    /// # }
    /// ```
    pub fn duration(&self) -> Duration {
        self.end.saturating_sub(self.start)
    }
}

/// Convert a count of 16kHz mono samples into an offset from stream start.
///
/// The engine forwards exactly one 320-sample frame per 20ms tick to every
/// local backend — zero-filled when a source has no audio — so a backend's own
/// running sample count *is* its position in the stream.
#[cfg(any(feature = "whisper", feature = "parakeet"))]
pub(crate) fn samples_to_duration(samples: u64) -> Duration {
    Duration::from_nanos(
        samples.saturating_mul(1_000_000_000) / crate::audio::resampler::OUT_SAMPLE_RATE as u64,
    )
}

// ---------------------------------------------------------------------------
// TranscriptStream
// ---------------------------------------------------------------------------

/// The consumer end of the transcript channel.
///
/// The channel is **bounded** (see
/// [`SplitStreamBuilder::channel_capacity`](crate::SplitStreamBuilder::channel_capacity),
/// default [`DEFAULT_CHANNEL_CAPACITY`](crate::DEFAULT_CHANNEL_CAPACITY)) with a **drop-oldest** overflow
/// policy: if the consumer falls behind, the oldest queued transcripts are
/// discarded and the newest are kept. Live captions are worth more than stale
/// ones, and an unbounded queue on a slow consumer is an out-of-memory bug
/// waiting for a long meeting.
///
/// Dropped segments are never silent: [`recv`](Self::recv) logs a warning
/// through the `log` crate and counts them in [`dropped`](Self::dropped).
///
/// # Example
///
// Compiled only when a backend this example can call is compiled in;
// `cargo test --no-default-features` would otherwise fail on a method
// that does not exist.
#[cfg_attr(feature = "parakeet", doc = "```no_run")]
#[cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
/// # async fn run() -> Result<(), splitstream::SplitStreamError> {
/// let (handle, mut transcripts) = splitstream::SplitStreamBuilder::new()
///     .with_parakeet("./models/parakeet-eou")
///     .channel_capacity(256)
///     .start()
///     .await?;
///
/// while let Some(t) = transcripts.recv().await {
///     println!("[{}] {}", t.source, t.text);
/// }
/// eprintln!("{} transcript(s) dropped", transcripts.dropped());
/// # handle.shutdown();
/// # Ok(())
/// # }
/// ```
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

/// What the next poll should do. `tokio::sync::broadcast` exposes no
/// `poll_recv`, only an `async fn recv`, so the only way to wait on it is to
/// hold a future. That future owns the receiver and hands it back when it
/// resolves, which is what lets this type keep a single cursor.
enum State {
    /// Holding the receiver, no wait in progress.
    Idle(Receiver<Transcript>),
    /// Waiting on `recv`. **The future is kept across polls**: dropping it
    /// would unlink the waker it registered and the next `send` would wake
    /// nobody.
    Receiving(Pin<Box<RecvOnce>>),
    /// Every sender is gone and the stream has ended.
    Done,
}

/// The future `State::Receiving` holds: it takes the receiver, awaits one
/// value, and returns the receiver alongside the result.
type RecvOnce = dyn Future<Output = (Result<Transcript, RecvError>, Receiver<Transcript>)> + Send;

fn recv_once(mut rx: Receiver<Transcript>) -> Pin<Box<RecvOnce>> {
    Box::pin(async move {
        let result = rx.recv().await;
        (result, rx)
    })
}

/// The consumer end of the transcript channel.
///
/// The channel is **bounded** (see
/// [`SplitStreamBuilder::channel_capacity`](crate::SplitStreamBuilder::channel_capacity),
/// default [`DEFAULT_CHANNEL_CAPACITY`](crate::DEFAULT_CHANNEL_CAPACITY)) with a
/// **drop-oldest** overflow
/// policy: if the consumer falls behind, the oldest queued transcripts are
/// discarded and the newest are kept. Live captions are worth more than stale
/// ones, and an unbounded queue on a slow consumer is an out-of-memory bug
/// waiting for a long meeting.
///
/// Dropped segments are never silent: both [`recv`](Self::recv) and the
/// [`Stream`](futures_core::Stream) implementation log a warning through the
/// `log` crate and count them in [`dropped`](Self::dropped).
///
/// Await it with [`recv`](Self::recv), or use it as a
/// [`Stream`](futures_core::Stream); both share one cursor, so pick one and
/// stay with it rather than interleaving the two.
///
/// # Example
///
// Compiled only when a backend this example can call is compiled in;
// `cargo test --no-default-features` would otherwise fail on a method
// that does not exist.
#[cfg_attr(feature = "parakeet", doc = "```no_run")]
#[cfg_attr(not(feature = "parakeet"), doc = "```ignore")]
/// # async fn run() -> Result<(), splitstream::SplitStreamError> {
/// let (handle, mut transcripts) = splitstream::SplitStreamBuilder::new()
///     .with_parakeet("./models/parakeet-eou")
///     .channel_capacity(256)
///     .start()
///     .await?;
///
/// while let Some(t) = transcripts.recv().await {
///     println!("[{}] {}", t.source, t.text);
/// }
/// eprintln!("{} transcript(s) dropped", transcripts.dropped());
/// # handle.shutdown();
/// # Ok(())
/// # }
/// ```
pub struct TranscriptStream {
    state: State,
    /// Kept only so [`resubscribe`](Self::resubscribe) works whatever the
    /// state. It is never polled, and an un-advancing receiver costs the
    /// channel nothing: `broadcast` senders overwrite rather than block.
    template: Receiver<Transcript>,
    dropped: u64,
}

impl std::fmt::Debug for TranscriptStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state {
            State::Idle(_) => "idle",
            State::Receiving(_) => "receiving",
            State::Done => "done",
        };
        f.debug_struct("TranscriptStream")
            .field("state", &state)
            .field("dropped", &self.dropped)
            .finish()
    }
}

impl TranscriptStream {
    pub(crate) fn new(rx: Receiver<Transcript>) -> Self {
        Self {
            template: rx.resubscribe(),
            state: State::Idle(rx),
            dropped: 0,
        }
    }

    /// Poll for the next transcript. The one code path behind both
    /// [`recv`](Self::recv) and [`Stream::poll_next`](futures_core::Stream).
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Transcript>> {
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::Idle(rx) => self.state = State::Receiving(recv_once(rx)),
                State::Receiving(mut fut) => match fut.as_mut().poll(cx) {
                    // Put the future back. This is the whole point: the waker
                    // it registered stays registered.
                    Poll::Pending => {
                        self.state = State::Receiving(fut);
                        return Poll::Pending;
                    }
                    Poll::Ready((Ok(t), rx)) => {
                        self.state = State::Idle(rx);
                        return Poll::Ready(Some(t));
                    }
                    Poll::Ready((Err(RecvError::Closed), _)) => {
                        self.state = State::Done;
                        return Poll::Ready(None);
                    }
                    Poll::Ready((Err(RecvError::Lagged(n)), rx)) => {
                        self.dropped = self.dropped.saturating_add(n);
                        log::warn!(
                            "transcript channel overflowed: {n} transcript(s) dropped \
                             ({} total) — the consumer is not keeping up, or raise \
                             SplitStreamBuilder::channel_capacity",
                            self.dropped
                        );
                        self.state = State::Idle(rx);
                    }
                },
                State::Done => return Poll::Ready(None),
            }
        }
    }

    /// Wait for the next transcript.
    ///
    /// Returns `None` once the engine has shut down and every queued transcript
    /// has been delivered, so this reads naturally as
    /// `while let Some(t) = transcripts.recv().await`.
    ///
    /// If the consumer fell behind and the channel dropped its oldest entries,
    /// this logs a warning, adds them to [`dropped`](Self::dropped), and
    /// returns the oldest transcript that survived.
    ///
    /// Cancelling this future loses nothing: the in-flight wait is kept, so a
    /// `select!` arm that loses the race resumes where it left off.
    pub async fn recv(&mut self) -> Option<Transcript> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// How many transcripts the drop-oldest policy has discarded so far, as
    /// observed by this stream.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// A second, independent view of the same transcripts, starting from now.
    ///
    /// Each view has its own position and its own lag accounting, so a slow
    /// logger cannot stall a fast UI.
    pub fn resubscribe(&self) -> Self {
        Self::new(self.template.resubscribe())
    }
}

impl From<Receiver<Transcript>> for TranscriptStream {
    fn from(rx: Receiver<Transcript>) -> Self {
        Self::new(rx)
    }
}

impl futures_core::Stream for TranscriptStream {
    type Item = Transcript;

    /// Yields transcripts in order, ending once the engine has shut down.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Transcript>> {
        self.get_mut().poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_end_minus_start() {
        let t = Transcript::new(
            AudioSource::Mic,
            "hi".into(),
            true,
            Duration::from_millis(500),
            Duration::from_millis(1_500),
            Some(0.9),
        );
        assert_eq!(t.duration(), Duration::from_secs(1));
    }

    #[test]
    fn duration_never_underflows() {
        let t = Transcript::new(
            AudioSource::Sys,
            "hi".into(),
            true,
            Duration::from_secs(2),
            Duration::from_secs(1),
            None,
        );
        assert_eq!(t.duration(), Duration::ZERO);
    }

    /// Poll the `Stream` impl without pulling in `futures::StreamExt`.
    async fn next_via_stream(s: &mut TranscriptStream) -> Option<Transcript> {
        std::future::poll_fn(|cx| futures_core::Stream::poll_next(std::pin::Pin::new(&mut *s), cx))
            .await
    }

    fn segment(text: &str) -> Transcript {
        Transcript::new(
            AudioSource::Mic,
            text.into(),
            true,
            Duration::ZERO,
            Duration::from_millis(20),
            None,
        )
    }

    /// Regression test: `poll_next` must keep its in-flight `recv` future
    /// across polls. An implementation that rebuilds the future every poll
    /// drops the waker it registered, so the `send` below wakes nobody and the
    /// consumer parks forever.
    ///
    /// The timeout has to be generous *and* the assertion has to be about
    /// arrival, not elapsed time: a buggy implementation only ever advances
    /// when some unrelated waker fires, so a short timeout would also "pass"
    /// for the wrong reason on a slow machine.
    #[tokio::test]
    async fn the_stream_impl_is_woken_by_a_send() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut stream = TranscriptStream::new(rx);

        let waiter = tokio::spawn(async move {
            let got = next_via_stream(&mut stream).await;
            (got, stream.dropped())
        });

        // Park the consumer first, then send. If the waker was discarded, this
        // send is never observed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(segment("hello")).expect("receiver is alive");

        let (got, dropped) = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("a send must wake a parked Stream consumer")
            .expect("waiter task panicked");

        assert_eq!(got.expect("a transcript").text, "hello");
        assert_eq!(dropped, 0);
    }

    /// Dropping every sender ends the stream rather than parking it.
    #[tokio::test]
    async fn the_stream_impl_ends_when_the_senders_go() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut stream = TranscriptStream::new(rx);
        tx.send(segment("last")).unwrap();
        drop(tx);

        assert_eq!(next_via_stream(&mut stream).await.unwrap().text, "last");
        assert!(next_via_stream(&mut stream).await.is_none());
        // ...and stays ended.
        assert!(next_via_stream(&mut stream).await.is_none());
    }

    /// Overflow is counted and logged, not silently skipped, on the Stream path
    /// as well as through `recv`.
    #[tokio::test]
    async fn overflow_is_counted_on_the_stream_path() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Transcript>(2);
        let mut stream = TranscriptStream::new(rx);
        for n in 0..5 {
            tx.send(segment(&n.to_string())).unwrap();
        }

        // Oldest entries are gone; the first delivery is what survived.
        let first = next_via_stream(&mut stream).await.expect("a transcript");
        assert_eq!(first.text, "3", "drop-oldest keeps the newest entries");
        assert_eq!(stream.dropped(), 3);
    }

    /// A cancelled poll must not lose the in-flight wait.
    #[tokio::test]
    async fn an_abandoned_poll_resumes() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Transcript>(8);
        let mut stream = TranscriptStream::new(rx);

        // Poll once with nothing queued, then abandon that future.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), next_via_stream(&mut stream))
                .await
                .is_err(),
            "nothing sent yet, so the poll must still be pending"
        );

        tx.send(segment("after cancel")).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), next_via_stream(&mut stream))
            .await
            .expect("the value must still be there after an abandoned poll");
        assert_eq!(got.unwrap().text, "after cancel");
    }

    #[cfg(any(feature = "whisper", feature = "parakeet"))]
    #[test]
    fn sample_counts_convert_to_offsets() {
        assert_eq!(samples_to_duration(16_000), Duration::from_secs(1));
        assert_eq!(samples_to_duration(320), Duration::from_millis(20));
        assert_eq!(samples_to_duration(0), Duration::ZERO);
    }
}
