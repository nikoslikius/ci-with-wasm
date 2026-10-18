set shell := ["bash", "-euo", "pipefail", "-c"]

# Isolated state/data/cache for every recipe.
export XDG_STATE_HOME := justfile_directory() / "target/wasm-state"
export XDG_DATA_HOME  := justfile_directory() / "target/wasm-data"
export XDG_CACHE_HOME := justfile_directory() / "target/wasm-cache"

pdk      := "target/python-pdk/extism-py"
binaryen := "target/python-binaryen"
src      := "examples/python/plugin.py"
plugin   := "target/wasm-python/python.wasm"

# Separate target dir so the host build doesn't contend with a normal build.
host := "CARGO_TARGET_DIR=target/wasm-host cargo run -- --plugin " + plugin + " --plugin-wasi"

input := '{"pipeline":{"git_url":"https://github.com/dtolnay/anyhow.git","image":"rust:1.90-alpine"}}'

default:
    @just --list

python-pdk-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(uname -m)" = x86_64 ] || { echo "Python PDK setup supports x86_64 Linux only" >&2; exit 1; }
    mkdir -p target/python-pdk {{binaryen}}/bin target/downloads
    fetch() { # url sha256 archive
        curl -fL --retry 2 -o "target/downloads/$3" "$1"
        echo "$2  target/downloads/$3" | sha256sum -c -
    }
    if [ ! -x {{pdk}}/bin/extism-py ]; then
        fetch https://github.com/extism/python-pdk/releases/download/v0.1.5/extism-py-x86_64-linux-v0.1.5.tar.gz \
            24fa8a4a2f2fe25e3a95a4dc1dbd10a4c30e97697102fec45c59bfc743b2d9d4 extism-py.tar.gz
        tar -xzf target/downloads/extism-py.tar.gz -C target/python-pdk
    fi
    if [ ! -x {{binaryen}}/bin/wasm-opt ] || [ ! -x {{binaryen}}/bin/wasm-merge ]; then
        fetch https://github.com/WebAssembly/binaryen/releases/download/version_116/binaryen-version_116-x86_64-linux.tar.gz \
            c55b74f3109cdae97490faf089b0286d3bba926bb6ea5ed00c8c784fc53718fd binaryen.tar.gz
        tar -xzf target/downloads/binaryen.tar.gz -C {{binaryen}} --strip-components=1 \
            binaryen-version_116/bin/wasm-opt binaryen-version_116/bin/wasm-merge
    fi
    rm -f target/downloads/*.tar.gz

python-build: python-pdk-setup
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "$(dirname {{plugin}})"
    if [ ! -f {{plugin}} ] || [ {{src}} -nt {{plugin}} ]; then
        PATH="$PWD/{{binaryen}}/bin:$PATH" \
        EXTISM_PYTHON_WASI_DEPS_DIR="$PWD/{{pdk}}/share/extism-py" \
        {{pdk}}/bin/extism-py {{src}} -o {{plugin}}
    else
        echo "Python python WASM plugin is up to date"
    fi

python-run: python-build
    {{host}} --plugin-input '{{input}}'

python-serve: python-build
    {{host}} --serve
