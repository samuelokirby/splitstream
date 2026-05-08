use splitstream::{AudioSource, SplitStreamBuilder};

#[tokio::main]
async fn main() {
    // Download the Parakeet EOU ONNX model and point this path at the directory
    // containing decoder_joint.onnx, encoder.onnx, and tokenizer.json.
    // See: https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2
    let (handle, mut rx) = SplitStreamBuilder::new()
        .with_parakeet("./models/parakeet-eou")
        .echo_cancellation(true)
        .start()
        .await
        .expect("failed to start splitstream");

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown();
    });

    while let Some(t) = rx.recv().await {
        let label = match t.source {
            AudioSource::Mic => "[🎤 Microphone] ",
            AudioSource::Sys => "[🖥️  System Audio] ",
        };
        println!("{} {}", label, t.text);
    }
}
