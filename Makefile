all: build

build:
	cargo build --release

# 全静态 musl 构建（NixOS 交叉工具链，参考 ../hns 方案）
musl: build-musl-static

build-musl-static:
	nix-shell -p 'pkgsCross.musl64.stdenv.cc' --run 'CC_x86_64_unknown_linux_musl=x86_64-unknown-linux-musl-gcc cargo build --release --target x86_64-unknown-linux-musl'

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
