#!/bin/bash
# Builds the ATSC player (scanner/player) to WebAssembly for the Viewer: the module goes beside the
# UI's scripts (scanner/src/ui/static/player/atsc_player.wasm), and the scanner's test that it was
# built from the crate's current sources is blessed with their hash. Run after changing the crate,
# and commit the module and its .source with the change. Needs the wasm32-unknown-unknown target
# (rustup target add wasm32-unknown-unknown).
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here/scanner/player"
cargo build --release --target wasm32-unknown-unknown
mkdir -p ../src/ui/static/player
cp target/wasm32-unknown-unknown/release/atsc_player.wasm ../src/ui/static/player/atsc_player.wasm
cd ..
PLAYER_BLESS=1 cargo test --quiet the_player_module_is_built_from_its_sources >/dev/null
echo "scanner/src/ui/static/player/atsc_player.wasm: $(wc -c < src/ui/static/player/atsc_player.wasm) bytes, sources $(cat src/ui/static/player/atsc_player.source)"
