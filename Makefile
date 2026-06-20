.PHONY: all serve serve_debug sdl2 sdl2_debug clean publish

WASM_OUT         := static/pkg/rustrdp_wasm_bg.wasm
PROXY_OUT        := target/release/rustrdp-proxy
PROXY_DEBUG_OUT  := target/debug/rustrdp-proxy
SDL2_OUT         := target/release/rustrdp-sdl2
SDL2_DEBUG_OUT   := target/debug/rustrdp-sdl2

PROXY_ASSETS_DIR := rustrdp-proxy/assets
PROXY_ASSETS     := $(PROXY_ASSETS_DIR)/index.html \
                    $(PROXY_ASSETS_DIR)/pkg/rdp_wasm.js \
                    $(PROXY_ASSETS_DIR)/pkg/rdp_wasm_bg.wasm

WASM_SRCS  := $(shell find rustrdp-wasm/src rustrdp-core/src -name '*.rs') \
               rustrdp-wasm/Cargo.toml rustrdp-core/Cargo.toml Cargo.lock

PROXY_SRCS := $(shell find rustrdp-proxy/src rustrdp-core/src -name '*.rs') \
               rustrdp-proxy/Cargo.toml rustrdp-core/Cargo.toml Cargo.lock

SDL2_SRCS  := $(shell find rustrdp-sdl2/src rustrdp-core/src -name '*.rs') \
               rustrdp-sdl2/Cargo.toml rustrdp-core/Cargo.toml Cargo.lock

$(WASM_OUT): $(WASM_SRCS)
	wasm-pack build rustrdp-wasm --target web --out-dir ../static/pkg

$(PROXY_ASSETS_DIR)/index.html: static/index.html
	@mkdir -p $(PROXY_ASSETS_DIR)
	cp $< $@

$(PROXY_ASSETS_DIR)/pkg/rdp_wasm.js: static/pkg/rdp_wasm.js
	@mkdir -p $(PROXY_ASSETS_DIR)/pkg
	cp $< $@

$(PROXY_ASSETS_DIR)/pkg/rdp_wasm_bg.wasm: static/pkg/rdp_wasm_bg.wasm
	@mkdir -p $(PROXY_ASSETS_DIR)/pkg
	cp $< $@

$(PROXY_OUT): $(PROXY_SRCS) $(PROXY_ASSETS)
	cargo build -p rustrdp-proxy --release

$(PROXY_DEBUG_OUT): $(PROXY_SRCS) $(PROXY_ASSETS)
	cargo build -p rustrdp-proxy

$(SDL2_OUT): $(SDL2_SRCS)
	cargo build -p rustrdp-sdl2 --release

$(SDL2_DEBUG_OUT): $(SDL2_SRCS)
	cargo build -p rustrdp-sdl2

wasm: $(WASM_OUT)

proxy: $(PROXY_OUT)

all: wasm proxy

serve: $(WASM_OUT) $(PROXY_OUT)
	-lsof -ti :8081 | xargs kill -9 2>/dev/null; true
	$(PROXY_OUT) --listen 0.0.0.0:8081

serve_debug: $(WASM_OUT) $(PROXY_DEBUG_OUT)
	-lsof -ti :8081 | xargs kill -9 2>/dev/null; true
	$(PROXY_DEBUG_OUT) --listen 0.0.0.0:8081

sdl2: $(SDL2_OUT)
	$(SDL2_OUT)

sdl2_debug: $(SDL2_DEBUG_OUT)
	RUST_LOG=debug $(SDL2_DEBUG_OUT) 2>&1 | tee /tmp/rustrdp-debug.log

publish: $(WASM_OUT) $(PROXY_ASSETS)
	cargo publish -p rustrdp-proxy

clean:
	cargo clean
	rm -rf static/pkg $(PROXY_ASSETS_DIR)
