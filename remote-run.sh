#!/usr/bin/env bash

set -euo pipefail

export RUSTFLAGS="-C link-arg=-Tpal/pal_nrf/link.x -C linker-plugin-lto"
cargo build --target=thumbv7em-none-eabihf
scp target/thumbv7em-none-eabihf/debug/fobnail firmowy:/tmp/
ssh -t firmowy /home/akowalski/.local/bin/probe-run --chip nrf52840 /tmp/fobnail
