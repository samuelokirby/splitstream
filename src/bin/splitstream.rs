//! splitstream binary — reads settings.toml + .env and starts transcription.
//!
//! Run with all backends:
//!   cargo run --features whisper,parakeet
//!
//! Or rely on the default features (whisper + parakeet enabled by default):
//!   cargo run

use colored::*;
use config::{Config, File};
use serde::Deserialize;
use splitstream::{AudioSource, SplitStreamBuilder};

#[derive(Deserialize)]
struct Settings {
    compliance_mode_on_start: bool,
    echo_cancellation: bool,
    transcription_backend: String,
    #[cfg(feature = "whisper")]
    whisper_model_path: String,
    #[cfg(feature = "whisper")]
    whisper_window_seconds: u32,
    #[cfg(feature = "parakeet")]
    parakeet_model_dir: String,
}

impl Settings {
    fn load() -> Self {
        Config::builder()
            .add_source(File::with_name("settings"))
            .build()
            .expect("failed to read settings.toml")
            .try_deserialize()
            .expect("invalid settings.toml")
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let settings = Settings::load();

    print_banner();

    let mut builder = SplitStreamBuilder::new()
        .sys_muted(settings.compliance_mode_on_start)
        .echo_cancellation(settings.echo_cancellation);

    match settings.transcription_backend.as_str() {
        #[cfg(feature = "parakeet")]
        "parakeet" => {
            println!("Transcription backend: Parakeet (local ONNX)");
            builder = builder.with_parakeet(&settings.parakeet_model_dir);
        }
        #[cfg(feature = "whisper")]
        "whisper" => {
            println!("Transcription backend: Whisper (local Metal)");
            builder = builder.with_whisper_config(splitstream::WhisperConfig {
                model_path: settings.whisper_model_path,
                window_seconds: settings.whisper_window_seconds,
            });
        }
        _ => {
            println!("Transcription backend: Deepgram");
            let api_key = std::env::var("DEEPGRAM_API_KEY")
                .expect("DEEPGRAM_API_KEY not set — add it to .env or the environment");
            builder = builder.with_deepgram(api_key);
        }
    }

    let (handle, mut rx) = builder.start().await.expect("failed to start splitstream");

    let confirm_msg = "✅ Recording. Press Ctrl+C to stop.".green().bold();
    println!("{}", confirm_msg);

    // On Ctrl+C, shut down the engine cleanly.
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        handle.shutdown();
    });

    while let Some(transcript) = rx.recv().await {
        let label = match transcript.source {
            AudioSource::Mic => "🎙️ (mic)".white().to_string(),
            AudioSource::Sys => "🔊 (sys)".bright_black().to_string(),
        };
        let text = if transcript.is_final {
            transcript.text.green().bold().to_string()
        } else {
            transcript.text.clone()
        };
        println!("{}\t{}", label, text);
    }
}

fn print_banner() {
    let banner = "
███████╗██████╗ ██╗     ██╗████████╗███████╗████████╗██████╗ ███████╗ █████╗ ███╗   ███╗
██╔════╝██╔══██╗██║     ██║╚══██╔══╝██╔════╝╚══██╔══╝██╔══██╗██╔════╝██╔══██╗████╗ ████║
███████╗██████╔╝██║     ██║   ██║   ███████╗   ██║   ██████╔╝█████╗  ███████║██╔████╔██║
╚════██║██╔═══╝ ██║     ██║   ██║   ╚════██║   ██║   ██╔══██╗██╔══╝  ██╔══██║██║╚██╔╝██║
███████║██║     ███████╗██║   ██║   ███████║   ██║   ██║  ██║███████╗██║  ██║██║ ╚═╝ ██║
╚══════╝╚═╝     ╚══════╝╚═╝   ╚═╝   ╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝

    ";
    println!("\n\n{}", banner.bright_blue());
    println!("\t\tDemo");
    println!("\t\t© Secretary Corporation 2024-2025, all rights reserved.")
}
