# What building the Hubris bench needs, for a host that is not Linux on
# x86-64: zaqaru builds only there (its vDSO is x86 assembly, its arena
# Linux's mmap), and Humility and zaqaru-m0 have to be Linux x86-64
# programs to run in a container. build.sh uses this automatically.
FROM --platform=linux/amd64 rust:1.96-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends clang && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# Humility's own minimum.
RUN rustup toolchain install 1.98.1 --profile minimal
# bookworm's wasm-ld is LLVM 14, older than zaqaru's link wants; the Rust
# toolchain's own lld is current, and is wasm-ld under -flavor wasm.
RUN printf '#!/bin/sh\nexec "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld" -flavor wasm "$@"\n' \
    > /usr/local/bin/wasm-ld && chmod +x /usr/local/bin/wasm-ld
