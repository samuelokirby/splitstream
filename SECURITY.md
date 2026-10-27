# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| 0.2.x | ✅ |
| 0.1.x | ❌ — please upgrade |

Only the latest published minor version receives security fixes. `splitstream` is
pre-1.0; fixes ship in a new release rather than as backports.

## Reporting a vulnerability

**Do not open a public GitHub issue for a security problem.**

Report privately via GitHub's
[private vulnerability reporting](https://github.com/samuelokirby/splitstream/security/advisories/new)
on this repository, or by email to **sam@secretary.so**.

Please include:

- the version of `splitstream`, your macOS version, and the enabled cargo features;
- what an attacker gains, and what access they need to get it;
- a minimal reproduction if you have one.

**Response targets:** acknowledgement within 3 business days, an assessment within 10
business days, and a fix released within 90 days of confirmation. You will be credited
in the advisory and the CHANGELOG unless you would rather not be.

## Scope

`splitstream` captures live microphone and system audio, so the security-relevant
surface is mostly about that audio and the credentials used to transcribe it. In scope:

- audio, transcripts, or API keys leaking to an unintended destination;
- bypassing the mute / "compliance mode" control such that muted audio is still
  captured, transmitted, or transcribed;
- memory-unsafety in the `unsafe` CoreAudio tap and IOProc code, or in the FFI
  boundaries to `aec-rs`, `opus`, `whisper-rs`, and `parakeet-rs`;
- anything that causes audio capture without the macOS TCC consent prompts firing;
- path traversal, unchecked-download, or write-outside-destination issues in the
  `download-parakeet` binary.

Out of scope:

- vulnerabilities in third-party services (report Deepgram issues to Deepgram);
- the behaviour or output of third-party model weights, which carry their own licenses;
- issues that require an attacker to already have local code execution as the user;
- the fact that transcription requires audio access at all — that is the crate's purpose,
  and it is gated by macOS TCC consent.

## Notes for users

- The Deepgram backend sends captured audio to a third-party cloud service. The
  `parakeet` and `whisper` backends run entirely on-device and send nothing.
- Your `DEEPGRAM_API_KEY` is read from the environment or `.env`. `.env` is gitignored;
  keep it that way.
- Model weights fetched by `download-parakeet` come from a third party. Verify the
  pinned checksums if you are distributing a build.
