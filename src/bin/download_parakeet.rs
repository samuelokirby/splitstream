use std::fs;
use std::process::{Command, ExitCode};

const DEST: &str = "./models/parakeet-eou";
const BASE: &str =
    "https://huggingface.co/altunenes/parakeet-rs/resolve/main/realtime_eou_120m-v1-onnx";

fn download(filename: &str) -> bool {
    let url = format!("{BASE}/{filename}");
    let dest = format!("{DEST}/{filename}");
    println!("Downloading {filename}...");
    Command::new("curl")
        .args(["-L", "--progress-bar", &url, "-o", &dest])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn main() -> ExitCode {
    fs::create_dir_all(DEST).expect("failed to create models directory");

    let files = ["tokenizer.json", "decoder_joint.onnx", "encoder.onnx"];
    for file in &files {
        if !download(file) {
            eprintln!("Error: failed to download {file}");
            return ExitCode::FAILURE;
        }
    }

    println!("\nDone. Point splitstream at: {DEST}");
    ExitCode::SUCCESS
}
