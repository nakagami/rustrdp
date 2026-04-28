# rdpwasm

A Rust + WebAssembly MS RDP client.

## Architecture

Browser (WASM) ──WebSocket──► proxy (Rust) ──TCP──► RDP Server

## Crates

- `rdp-core`: Transport/rendering-agnostic RDP protocol library
- `rdp-wasm`: WebAssembly frontend using wasm-bindgen
- `proxy`: WebSocket-to-TCP proxy server

## Build

Install [wasm-pack](https://rustwasm.github.io/wasm-pack/) if not already available:

```sh
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
```

Then build and run:

```sh
make all
make serve
```
