//! `download-parakeet` — fetch the Parakeet Realtime EOU ONNX model splitstream
//! uses for the default `parakeet` backend.
//!
//! The model is **NVIDIA Parakeet-Realtime-EOU-120m-v1** (FastConformer-RNNT,
//! 120M parameters, English, streaming with end-of-utterance detection),
//! distributed under the [NVIDIA Open Model License][license] and exported to
//! ONNX by [`altunenes/parakeet-rs`][export]. Three files, ~481 MB total.
//!
//! [license]: https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/
//! [export]: https://huggingface.co/altunenes/parakeet-rs
//!
//! Every file is pinned to a specific Hugging Face **revision** and verified
//! against a hard-coded SHA-256 digest (via the `sha2` crate), so the bytes
//! cannot change under you.
//! Downloads land in `<name>.part` and are renamed only after the digest
//! matches.
//!
//! ```text
//! download-parakeet [--dest DIR] [--force] [--help]
//! ```

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Pinned source
// ---------------------------------------------------------------------------

/// Hugging Face repository holding the ONNX export.
const HF_REPO: &str = "altunenes/parakeet-rs";

/// Pinned repository revision. Never use `main`: it lets a third party change
/// the bytes users download after the fact.
const HF_REVISION: &str = "a61d2818df4659c956b9661a9447f46e98c15126";

/// Subdirectory inside the repository.
const HF_SUBDIR: &str = "realtime_eou_120m-v1-onnx";

/// Directory name created under the model root.
const MODEL_DIR_NAME: &str = "parakeet-eou";

/// One model file, with the digest it must hash to.
struct Artifact {
    name: &'static str,
    /// Lowercase hex SHA-256 of the file contents at [`HF_REVISION`].
    sha256: &'static str,
    /// Expected size in bytes, used only for progress reporting and an early
    /// mismatch message.
    size: u64,
}

/// The three files `parakeet-rs` loads from the model directory.
const ARTIFACTS: &[Artifact] = &[
    Artifact {
        name: "tokenizer.json",
        sha256: "f6b0ad8690559351fa478116fe0985a203b76f7c040f3a9381f485c99c0325f8",
        size: 20_053,
    },
    Artifact {
        name: "decoder_joint.onnx",
        sha256: "9d2553ac043c2fc5f69e970769b0fb8ab9103fbfdeb7d26a1ea9729d4bd2dddd",
        size: 21_347_639,
    },
    Artifact {
        name: "encoder.onnx",
        sha256: "d472887cc38a784a5bfc21c2dbe247639edc3b3f9992388d8ceceaec07256b5b",
        size: 459_341_289,
    },
];

// ---------------------------------------------------------------------------
// Destination resolution
// ---------------------------------------------------------------------------

/// Where the files go, in precedence order:
///
/// 1. `--dest DIR` — used verbatim as the model directory.
/// 2. `$SPLITSTREAM_MODEL_DIR` — treated as a model *root*; files land in
///    `<root>/parakeet-eou`.
/// 3. The platform's per-user application-data directory:
///    `~/Library/Application Support/splitstream/models/parakeet-eou` on macOS,
///    `%LOCALAPPDATA%\splitstream\models\parakeet-eou` on Windows.
///
/// The default is an absolute path on purpose. A CWD-relative `./models` is
/// unreachable from inside a bundled macOS `.app`, whose working directory is
/// `/`, and is equally wrong for a Windows app launched from a shortcut.
fn resolve_dest(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    if let Some(root) = std::env::var_os("SPLITSTREAM_MODEL_DIR") {
        return Ok(PathBuf::from(root).join(MODEL_DIR_NAME));
    }
    Ok(default_model_root()?.join(MODEL_DIR_NAME))
}

/// The per-user directory this platform keeps application data in.
#[cfg(target_os = "macos")]
fn default_model_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set — pass --dest DIR explicitly".to_string())?;
    Ok(PathBuf::from(home).join("Library/Application Support/splitstream/models"))
}

/// `%LOCALAPPDATA%` is the right home for a half-gigabyte of model weights:
/// unlike `%APPDATA%` it is excluded from roaming profiles, so a domain login
/// does not try to copy the models over the network.
#[cfg(target_os = "windows")]
fn default_model_root() -> Result<PathBuf, String> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local).join("splitstream\\models"));
    }
    // Fall back the way Windows itself does before giving up.
    let profile = std::env::var_os("USERPROFILE").ok_or_else(|| {
        "neither LOCALAPPDATA nor USERPROFILE is set — pass --dest DIR explicitly".to_string()
    })?;
    Ok(PathBuf::from(profile).join("AppData\\Local\\splitstream\\models"))
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn download(
    client: &reqwest::blocking::Client,
    artifact: &Artifact,
    dir: &Path,
) -> Result<(), String> {
    let name = artifact.name;
    let final_path = dir.join(name);
    let part_path = dir.join(format!("{name}.part"));

    let url = format!("https://huggingface.co/{HF_REPO}/resolve/{HF_REVISION}/{HF_SUBDIR}/{name}");

    print!("Downloading {name}... ");
    std::io::stdout().flush().ok();

    let mut response = client
        .get(&url)
        .send()
        .map_err(|e| format!("request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} for {url}", response.status()));
    }

    let total = response.content_length().unwrap_or(artifact.size);

    // Write to `<name>.part` so an interrupted or corrupt download never
    // replaces a good file, and never leaves a truncated one in its place.
    let mut file = File::create(&part_path)
        .map_err(|e| format!("could not create {}: {e}", part_path.display()))?;

    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut buf = vec![0u8; 1 << 16];

    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("write error: {e}"))?;
        downloaded += n as u64;
        print!(
            "\rDownloading {name}... {:.1} MB / {:.1} MB",
            downloaded as f64 / 1e6,
            total as f64 / 1e6
        );
        std::io::stdout().flush().ok();
    }

    file.flush().map_err(|e| format!("flush error: {e}"))?;
    drop(file);

    let digest = finalize_hex(hasher);
    if digest != artifact.sha256 {
        let _ = fs::remove_file(&part_path);
        return Err(format!(
            "checksum mismatch\n  expected sha256 {}\n  got      sha256 {digest}\n\
             The file at {url} is not the pinned revision. \
             Nothing was written.",
            artifact.sha256
        ));
    }

    fs::rename(&part_path, &final_path)
        .map_err(|e| format!("could not move {} into place: {e}", part_path.display()))?;

    println!(
        "\rDownloading {name}... done ({:.1} MB, sha256 verified)          ",
        downloaded as f64 / 1e6
    );
    Ok(())
}

/// True if `path` already holds exactly the expected bytes.
fn already_valid(path: &Path, artifact: &Artifact) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    finalize_hex(hasher) == artifact.sha256
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const HELP: &str = "\
download-parakeet — fetch the Parakeet Realtime EOU ONNX model for splitstream

USAGE:
    download-parakeet [--dest DIR] [--force]

OPTIONS:
    --dest DIR   Write the three model files into DIR.
    --force      Re-download even if the files are already present and valid.
    -h, --help   Print this help.

DESTINATION (first match wins):
    --dest DIR
    $SPLITSTREAM_MODEL_DIR/parakeet-eou
    macOS:   ~/Library/Application Support/splitstream/models/parakeet-eou
    Windows: %LOCALAPPDATA%\\splitstream\\models\\parakeet-eou

MODEL:
    NVIDIA Parakeet-Realtime-EOU-120m-v1 (120M params, English, streaming ASR
    with end-of-utterance detection), ONNX export from huggingface.co/altunenes/parakeet-rs.
    ~481 MB across three files, each pinned to a revision and verified by SHA-256.
    Weights are licensed under the NVIDIA Open Model License, which permits
    commercial use: https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/
";

struct Args {
    dest: Option<PathBuf>,
    force: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut dest = None;
    let mut force = false;
    let mut argv = std::env::args().skip(1);

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--force" => force = true,
            "--dest" => {
                let value = argv.next().ok_or("--dest requires a directory argument")?;
                dest = Some(PathBuf::from(value));
            }
            other => {
                if let Some(value) = other.strip_prefix("--dest=") {
                    dest = Some(PathBuf::from(value));
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
    }

    Ok(Some(Args { dest, force }))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("Error: {e}\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };

    let dest = match resolve_dest(args.dest) {
        Ok(dest) => dest,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = fs::create_dir_all(&dest) {
        eprintln!(
            "Error: could not create the model directory {}: {e}",
            dest.display()
        );
        return ExitCode::FAILURE;
    }

    let client = match reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(concat!("splitstream/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Error: could not build the HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "Model:    NVIDIA Parakeet-Realtime-EOU-120m-v1 (ONNX, ~481 MB)\n\
         Source:   huggingface.co/{HF_REPO} @ {rev}\n\
         License:  NVIDIA Open Model License (commercial use permitted)\n\
         Dest:     {dest}\n",
        rev = &HF_REVISION[..12],
        dest = dest.display()
    );

    for artifact in ARTIFACTS {
        let path = dest.join(artifact.name);
        if !args.force && already_valid(&path, artifact) {
            println!(
                "Downloading {}... already present, sha256 verified",
                artifact.name
            );
            continue;
        }
        if let Err(e) = download(&client, artifact, &dest) {
            eprintln!("\nError downloading {}: {e}", artifact.name);
            return ExitCode::FAILURE;
        }
    }

    println!("\nDone. Point splitstream at this directory:\n");
    println!("    .with_parakeet(\"{}\")\n", dest.display());
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Digest helper
// ---------------------------------------------------------------------------

/// Lowercase hex rendering of a finished SHA-256 digest, in the same form as
/// the pinned [`Artifact::sha256`] constants.
fn finalize_hex(hasher: Sha256) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        finalize_hex(h)
    }

    /// Guards the `Sha256` → lowercase-hex wiring, not the crypto itself:
    /// a byte-order or formatting slip here would silently reject every
    /// correctly downloaded file.
    #[test]
    fn nist_vectors() {
        assert_eq!(
            hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hash(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// The downloader hashes the stream in 64 KiB reads, so the incremental
    /// path is the one that actually runs in production.
    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let mut h = Sha256::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(finalize_hex(h), hash(&data));
    }

    #[test]
    fn already_valid_rejects_a_missing_file() {
        assert!(!already_valid(
            Path::new("/nonexistent/splitstream/tokenizer.json"),
            &ARTIFACTS[0]
        ));
    }

    #[test]
    fn already_valid_rejects_wrong_contents() {
        let dir = std::env::temp_dir().join("splitstream-dl-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokenizer.json");
        fs::write(&path, b"not the model").unwrap();
        assert!(!already_valid(&path, &ARTIFACTS[0]));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn artifact_digests_are_well_formed() {
        for a in ARTIFACTS {
            assert_eq!(a.sha256.len(), 64, "{}", a.name);
            assert!(
                a.sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{} digest must be lowercase hex",
                a.name
            );
        }
    }

    #[test]
    fn revision_is_pinned_not_a_branch() {
        assert_eq!(HF_REVISION.len(), 40);
        assert!(HF_REVISION.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
