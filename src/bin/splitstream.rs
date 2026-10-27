//! `splitstream` demo binary — reads `settings.toml` (and `.env`) from the
//! current directory and streams transcripts to the terminal.
//!
//! Both binaries are gated behind the `cli` feature:
//!
//! ```text
//! cargo run --features cli                       # default backend: parakeet
//! cargo run --features cli,deepgram              # add the Deepgram backend
//! ```
//!
//! The crate's default feature set is `["parakeet"]` — Deepgram and Whisper are
//! opt-in.

use colored::*;
use config::{Config, File};
use serde::Deserialize;
use splitstream::{AudioSource, SplitStreamBuilder};

#[derive(Deserialize)]
struct Settings {
    compliance_mode_on_start: bool,
    echo_cancellation: bool,
    transcription_backend: String,
    #[cfg(feature = "parakeet")]
    parakeet_model_dir: String,
    /// Undocumented: the Whisper backend is a work in progress and is not
    /// listed in `settings.toml`.
    #[cfg(feature = "whisper")]
    #[serde(default)]
    whisper_model_path: Option<String>,
}

impl Settings {
    /// Load `settings.toml` from the current directory, exiting with a clear
    /// message rather than panicking when it is missing or malformed.
    fn load() -> Self {
        let config = Config::builder()
            .add_source(File::with_name("settings"))
            .build()
            .unwrap_or_else(|e| {
                eprintln!(
                    "Could not read settings.toml from the current directory: {e}\n\
                     Create one next to where you run this binary — see the README."
                );
                std::process::exit(1);
            });

        config.try_deserialize().unwrap_or_else(|e| {
            eprintln!("settings.toml is not valid: {e}");
            std::process::exit(1);
        })
    }
}

// With no backend feature compiled in, every arm of the backend `match` below
// diverges (there is only the fallback), so the builder is never used and
// everything after the match is dead. That is the correct behaviour for a
// binary built without a backend; it just needs to not be a warning.
#[cfg_attr(
    not(any(feature = "deepgram", feature = "whisper", feature = "parakeet")),
    allow(unreachable_code, unused_mut, unused_variables)
)]
#[tokio::main]
async fn main() {
    // The library logs through `log` and never prints, so without a subscriber
    // every warning and error it emits would be invisible.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("splitstream=info"))
        .format_timestamp(None)
        .init();

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
            builder = builder.with_parakeet(settings.parakeet_model_dir.as_str());
        }
        #[cfg(feature = "whisper")]
        "whisper" => {
            println!("Transcription backend: Whisper (local Metal, work in progress)");
            let Some(path) = settings.whisper_model_path.as_deref() else {
                eprintln!(
                    "settings.toml selects the whisper backend but sets no whisper_model_path"
                );
                std::process::exit(1);
            };
            builder = builder.with_whisper(path);
        }
        #[cfg(feature = "deepgram")]
        "deepgram" => {
            println!("Transcription backend: Deepgram");
            let Ok(api_key) = std::env::var("DEEPGRAM_API_KEY") else {
                eprintln!("DEEPGRAM_API_KEY is not set — add it to .env or export it");
                std::process::exit(1);
            };
            builder = builder.with_deepgram(api_key);
        }
        _ => {
            eprintln!(
                "Unknown or unsupported backend: '{}'. Enable the matching feature flag.",
                settings.transcription_backend
            );
            std::process::exit(1);
        }
    }

    let (handle, mut rx) = match builder.start().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{} {e}", "Failed to start splitstream:".red().bold());
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            std::process::exit(1);
        }
    };

    // Failures that happen after start-up — a dropped socket, a backend that
    // dies — arrive here. Without this the session would just go quiet.
    if let Some(mut errors) = handle.errors() {
        tokio::spawn(async move {
            while let Some(e) = errors.recv().await {
                eprintln!("{} {e}", "splitstream:".red().bold());
                let mut source = std::error::Error::source(&e);
                while let Some(cause) = source {
                    eprintln!("  caused by: {cause}");
                    source = cause.source();
                }
            }
        });
    }

    let confirm_msg = "✅ Recording. Press Ctrl+C to stop.".green().bold();
    println!("{}", confirm_msg);

    // On Ctrl+C, shut down the engine cleanly. The handle is `Clone`, so the
    // original stays alive here and the session is not shut down early by its
    // drop.
    let stopper = handle.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        stopper.shutdown();
    });

    while let Some(transcript) = rx.recv().await {
        let label = match transcript.source {
            AudioSource::Mic => "🎙️ (mic)".white().to_string(),
            AudioSource::Sys => "🔊 (sys)".bright_black().to_string(),
            _ => "❔ (other)".bright_black().to_string(),
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
    println!("\t\t© 2026 samuelokirby")
}
