//! Integration tests exercising splitstream's public surface the way a
//! downstream crate sees it. Anything that needs real audio hardware lives in
//! `tests/pipeline.rs` behind `#[ignore]`.

use splitstream::{AudioSource, SplitStreamBuilder, SplitStreamError};

#[test]
fn audio_source_is_matchable_and_displayable() {
    // `AudioSource` is `#[non_exhaustive]`, so downstream matches need a
    // catch-all arm. This test exists to keep that contract honest.
    let label = match AudioSource::Mic {
        AudioSource::Mic => "you",
        AudioSource::Sys => "them",
        _ => "unknown",
    };
    assert_eq!(label, "you");

    assert_ne!(AudioSource::Mic, AudioSource::Sys);
    assert_eq!(AudioSource::Mic.to_string(), "mic");
    assert_eq!(AudioSource::Sys.to_string(), "sys");
}

/// With a backend feature compiled in, selecting none is `NoBackend`.
#[cfg(any(feature = "parakeet", feature = "deepgram", feature = "whisper"))]
#[tokio::test]
async fn start_without_a_backend_is_an_error() {
    let result = SplitStreamBuilder::new().start().await;
    assert!(matches!(result, Err(SplitStreamError::NoBackend)));
}

/// Built with no backend at all, the builder says so instead of blaming the
/// caller for not choosing one.
#[cfg(not(any(feature = "parakeet", feature = "deepgram", feature = "whisper")))]
#[tokio::test]
async fn start_without_a_backend_feature_is_an_error() {
    let result = SplitStreamBuilder::new().start().await;
    assert!(matches!(result, Err(SplitStreamError::NoBackendFeature)));
}

#[cfg(feature = "parakeet")]
#[tokio::test]
async fn missing_model_directory_names_the_path_and_the_fix() {
    let missing = std::env::temp_dir().join("splitstream-no-such-model-dir");
    let result = SplitStreamBuilder::new()
        .with_parakeet(&missing)
        .start()
        .await;

    match result {
        Err(SplitStreamError::ModelNotFound { path }) => assert_eq!(path, missing),
        other => panic!("expected ModelNotFound, got {other:?}"),
    }

    // The message must tell the reader how to fix it, not just what failed.
    let msg = SplitStreamError::ModelNotFound { path: missing }.to_string();
    assert!(
        msg.contains("download-parakeet"),
        "unhelpful message: {msg}"
    );
}

#[test]
fn errors_are_send_sync_and_static() {
    fn assert_bounds<T: Send + Sync + 'static>() {}
    assert_bounds::<SplitStreamError>();
    assert_bounds::<splitstream::Transcript>();
    assert_bounds::<splitstream::SplitStreamHandle>();
}
