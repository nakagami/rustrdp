# rustrdp

A Rust + WebAssembly MS RDP client.

## Architecture

```
Browser (WASM) ──WebSocket──► rustrdp-proxy (Rust) ──TCP──► RDP Server
```

## Crates

- `rustrdp-core`: Transport/rendering-agnostic RDP protocol library
- `rustrdp-wasm`: WebAssembly frontend using wasm-bindgen
- `rustrdp-proxy`: WebSocket-to-TCP proxy + static web server
- `rustrdp-sdl2`: Native desktop RDP client using SDL2

## Web client (rustrdp-proxy)

### Build and run (development)

Install [wasm-pack](https://rustwasm.github.io/wasm-pack/) if not already available:

```sh
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
```

Then build and run:

```sh
make serve
```

Open http://localhost:8081 in a browser.

### Install from crates.io

```sh
cargo install rustrdp-proxy
rustrdp-proxy
```

Open http://localhost:8081 in a browser.

## Desktop client (rustrdp-sdl2)

See [rustrdp-sdl2/README.md](rustrdp-sdl2/README.md) for installation and usage.

```sh
cargo install rustrdp-sdl2
```
