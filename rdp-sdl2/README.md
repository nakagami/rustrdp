# rdp-sdl2: Rust RDP Client with SDL2

A minimal RDP (Remote Desktop Protocol) client built in Rust using SDL2 for UI rendering and `rdp-core` for protocol implementation.

## Features

- **Minimal SDL2-based GUI**: Renders remote desktop framebuffers
- **Basic Input Support**: Keyboard and mouse input forwarding
- **Environment-based Configuration**: Connect via environment variables
- **Cross-platform**: Works on macOS, Linux, and Windows (with SDL2 installed)

## Requirements

SDL2 must be installed on your system. Follow the [SDL2 installation guide](https://wiki.libsdl.org/SDL2/Installation).

### macOS
```bash
brew install sdl2 sdl2_ttf
```

### Linux (Ubuntu/Debian)
```bash
sudo apt-get install libsdl2-dev libsdl2-ttf-dev
```

### Windows
Download from [SDL2 releases](https://github.com/libsdl-org/SDL/releases)

## Configuration

Connection parameters are specified via environment variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `GRDP_HOST` | RDP server hostname or IP address | `myserver.example.com` |
| `GRDP_PORT` | RDP server port | `3389` |
| `GRDP_USER` | Username | `user` |
| `GRDP_PASSWORD` | Password | `password` |
| `GRDP_DOMAIN` | Domain (optional) | `WORKGROUP` |
| `GRDP_WINDOW_SIZE` | Window size in WxH format | `1280x800` |

## Usage

```bash
export GRDP_HOST=myserver
export GRDP_PORT=3389
export GRDP_USER=user
export GRDP_PASSWORD=password
export GRDP_WINDOW_SIZE=1280x800

cargo run -p rdp-sdl2
```

## Building

```bash
cargo build -p rdp-sdl2 --release
```

The binary will be available at `target/release/rdp-sdl2`.

## Input Handling

- **Escape Key**: Close the application
- **Mouse**: Move and click
- **Keyboard**: US English layout

## Architecture

- `config.rs`: Environment variable configuration loading
- `connection.rs`: RDP connection management and Transport implementation
- `input.rs`: Keyboard and mouse event handling
- `ui.rs`: SDL2 UI rendering
- `main.rs`: Main event loop

## Limitations

- US keyboard layout only (no multi-layout support yet)
- No TLS/SSL support (requires proxy or direct connection)
- Basic rendering (32-bit RGBA only)
- No clipboard or printer redirection

## Notes

- This is a minimal implementation focusing on core functionality
- For production use, consider full RDP client implementations like FreeRDP
- The implementation uses blocking I/O; async improvements are possible for future versions
