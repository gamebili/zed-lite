#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
target_directory="${CARGO_TARGET_DIR:-$repository_root/target}"
if [[ "$target_directory" != /* ]]; then
    target_directory="$PWD/$target_directory"
fi

(
    cd -- "$repository_root"
    export CARGO_INCREMENTAL=1
    cargo build \
        --package zed \
        --bin zed \
        --profile release-local \
        --target-dir "$target_directory" \
        --config 'profile.release-local.lto="off"'
)

exec "$target_directory/release-local/zed" "$@"
