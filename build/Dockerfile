FROM 305232526136.dkr.ecr.us-east-2.amazonaws.com/mirror/rustlang/rust:nightly

WORKDIR /tmp

COPY rust-toolchain .

RUN rustup default "$(cat public/rust-toolchain)"; rm rust-toolchain

RUN set -eux; \
    apt-get update; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        docker.io \
        ca-certificates \
        build-essential \
        llvm \
        clang \
        libclang-dev \
        lld \
        cmake \
        libssl-dev \
        liblz4-dev \
        curl \
        protobuf-compiler \
        rsync; \
    rm -rf /var/lib/apt/lists/*

RUN curl -L https://github.com/docker/compose/releases/download/v2.6.0/docker-compose-linux-`uname -m` -o /usr/local/bin/docker-compose; \
    chmod a+x /usr/local/bin/docker-compose

RUN set -eux; \
    cargo install cargo-cache --version 0.6.3; \
    cargo install --git https://github.com/paritytech/cachepot --rev 2c4b14f4164ae954a1e6241e14c13feada0f1758; \
    cargo install detect_flake; \
    cargo install critcmp; \
    cargo cache --remove-dir all,git-db; \
    mkdir -p ~/.config/cachepot; touch ~/.config/cachepot/config # To clean up cachepot logs

# CARGO_HOME is set by the nightly container to be /usr/local/cargo.
# This is where it reads configs from for cargo and where cargo installs binaries.
RUN printf '[build]\nrustc-wrapper="/usr/local/cargo/bin/cachepot"\nrustflags = ["-C", "link-arg=-fuse-ld=lld"]' > ${CARGO_HOME}/config
