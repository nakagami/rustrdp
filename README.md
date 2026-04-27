# rdpwasm

A Rust + WebAssembly MS RDP client.

## Architecture

Browser (WASM) ──WebSocket──► proxy (Rust) ──TCP──► RDP Server

## Crates

- `rdp-core`: Transport/rendering-agnostic RDP protocol library
- `rdp-wasm`: WebAssembly frontend using wasm-bindgen
- `proxy`: WebSocket-to-TCP proxy server

## Build

make all
make serve
