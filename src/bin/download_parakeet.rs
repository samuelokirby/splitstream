use std::fs::{self, File};
use std::io::Write;
use std::process::ExitCode;

const DEST: &str = "./models/parakeet-eou";
const BASE: &str =
    "https://huggingface.co/altunenes/parakeet-rs/resolve/main/realtime_eou_120m-v1-onnx";

fn download(client: &reqwest::blocking::Client, filename: &str) -> Result<(), String> {
    let url = format!("{BASE}/{filename}");
    let dest = format!("{DEST}/{filename}");

    print!("Downloading {filename}... ");
    std::io::stdout().flush().ok();

    let mut response = client
        .get(&url)
        .send()
        .map_err(|e| format!("request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let total = response.content_length();
    let mut file = File::create(&dest).map_err(|e| format!("could not create file: {e}"))?;
    let mut downloaded: u64 = 0;
    let mut buf = [0u8; 65536];

    loop {
        use std::io::Read;
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write error: {e}"))?;
        downloaded += n as u64;
        if let Some(t) = total {
            print!("\rDownloading {filename}... {:.1} MB / {:.1} MB", downloaded as f64 / 1e6, t as f64 / 1e6);
            std::io::stdout().flush().ok();
        }
    }

    println!("\rDownloading {filename}... done ({:.1} MB)    ", downloaded as f64 / 1e6);
    Ok(())
}

fn main() -> ExitCode {
    fs::create_dir_all(DEST).expect("failed to create models directory");

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("splitstream/download-parakeet")
        .build()
        .expect("failed to build HTTP client");

    let files = ["tokenizer.json", "decoder_joint.onnx", "encoder.onnx"];
    for file in &files {
        if let Err(e) = download(&client, file) {
            eprintln!("Error downloading {file}: {e}");
            return ExitCode::FAILURE;
        }
    }

    println!("\nDone. Models saved to: {DEST}");
    ExitCode::SUCCESS
}
