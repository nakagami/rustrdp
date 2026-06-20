# rustrdp-proxy: Web RDP Proxy Server

A WebSocket-to-TCP proxy + static web server that serves the `rustrdp-wasm` browser client.

## Features

- **Self-contained binary**: HTML, WASM, and JS assets are embedded at build time
- **WebSocket proxy**: Bridges the browser to an RDP server over TCP/TLS
- **TLS support**: Automatically upgrades the RDP connection to TLS when required

## Install

```sh
cargo install rustrdp-proxy
```

## Usage

```sh
rustrdp-proxy
```

Open http://localhost:8081 in a browser, enter the RDP server address, and connect.

### Options

```
--listen <ADDR>   Listen address (default: 0.0.0.0:8081)
--static <DIR>    Override static files directory (for development)
```

## Development

Install [wasm-pack](https://rustwasm.github.io/wasm-pack/):

```sh
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
```

Build and run (WASM is rebuilt automatically when sources change):

```sh
make serve
```

## Architecture

```
Browser (WASM) ──WebSocket /ws──► rustrdp-proxy ──TCP/TLS──► RDP Server
                    HTTP /        (embedded assets)
```

The proxy handles the X.224 handshake and optional TLS upgrade transparently,
so the WASM client communicates over plain WebSocket binary frames.
