use splitstream::{AudioSource, SplitStreamBuilder};

#[tokio::main]
async fn main() {
    let (handle, mut rx) = SplitStreamBuilder::new()
        .with_parakeet("/Users/sam/Documents/Projects/splitstream/models/parakeet-eou")
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
            AudioSource::Mic => "[mic]",
            AudioSource::Sys => "[sys]",
        };
        println!("{} {}", label, t.text);
    }
}
