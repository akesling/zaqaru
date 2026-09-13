FROM rust:bookworm
RUN apt-get update && apt-get install -y --no-install-recommends clang lld python3 gcc libc6-dev util-linux && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
RUN ln -s "$(find /usr/local/rustup -path '*/bin/gcc-ld/wasm-ld' -print -quit)" /usr/local/bin/wasm-ld
ENV ZAQARU_BENCH_ENV=docker-linux-amd64
WORKDIR /work
ENTRYPOINT ["python3", "tools/microbench/measure.py"]
