#!/bin/bash
# Downloads the Parakeet EOU ONNX model files into ./models/parakeet-eou/
# Usage: bash scripts/download_parakeet.sh

set -e

DEST="./models/parakeet-eou"
BASE="https://huggingface.co/altunenes/parakeet-rs/resolve/main/realtime_eou_120m-v1-onnx"

mkdir -p "$DEST"

echo "Downloading Parakeet EOU model into $DEST..."
curl -L --progress-bar "$BASE/tokenizer.json"     -o "$DEST/tokenizer.json"
curl -L --progress-bar "$BASE/decoder_joint.onnx" -o "$DEST/decoder_joint.onnx"
curl -L --progress-bar "$BASE/encoder.onnx"       -o "$DEST/encoder.onnx"

echo "Done. Point splitstream at: $DEST"
