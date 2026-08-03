all: build

build:
	cargo build --release

musl: build-musl-static

build-musl-static:
	cargo build --release --target x86_64-unknown-linux-musl

debug:
	cargo build

test:
	cargo test

clean:
	cargo clean

install: build
	cp -rf ./target/release/simdict ~/.local/bin/

install-linux: build-musl-static
	cp ./target/x86_64-unknown-linux-musl/release/simdict ~/.local/bin/
