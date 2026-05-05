.PHONY: all wasm proxy serve clean

all: wasm proxy

wasm:
	wasm-pack build rdp-wasm --target web --out-dir ../static/pkg

proxy:
	cargo build -p proxy --release

serve:
	./target/release/proxy --listen 0.0.0.0:8081 --static static

clean:
	cargo clean
	rm -rf static/pkg
