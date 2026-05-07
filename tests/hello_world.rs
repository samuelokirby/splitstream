use splitstream::{AudioSource, Transcript};

#[test]
fn public_types_are_usable() {
    let t = Transcript {
        source: AudioSource::Mic,
        text: "hello".to_string(),
        is_final: true,
    };
    assert_eq!(t.text, "hello");
    assert!(t.is_final);
    assert_eq!(t.source, AudioSource::Mic);

    let cloned = t.clone();
    assert_eq!(cloned.text, t.text);
}

#[test]
fn audio_source_variants_are_distinct() {
    assert_ne!(AudioSource::Mic, AudioSource::Sys);
}
