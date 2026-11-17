#!/usr/bin/env bash
set -euo pipefail

ROOT=$(realpath "$(dirname "$0")/..")
PROJECT="$ROOT/zmusic-horizon"
REV=$(grep '^horizon = ' "$PROJECT/horizon-mod.toml" | cut -d '"' -f 2)
if [[ -z "${HORIZON_HOME:-}" ]]; then
    HORIZON_HOME="$ROOT/.gradle/horizon-sdk"
    managed=true
else
    managed=false
fi

if [[ ! "$REV" =~ ^[0-9a-f]{40}$ ]]; then
    echo "Invalid Horizon revision: $REV" >&2
    exit 1
fi

cloned=false
if [[ ! -d "$HORIZON_HOME/.git" ]]; then
    git clone --filter=blob:none https://github.com/horizon-rust/horizon.git "$HORIZON_HOME"
    cloned=true
fi
if [[ $(git -C "$HORIZON_HOME" rev-parse HEAD) != "$REV" ]]; then
    if [[ "$cloned" != true && "$managed" != true ]]; then
        echo "$HORIZON_HOME is not at required Horizon revision $REV" >&2
        exit 1
    fi
    git -C "$HORIZON_HOME" fetch --depth 1 origin "$REV"
    git -C "$HORIZON_HOME" checkout --detach "$REV"
fi

mkdir -p "$PROJECT/.horizon/sdk"
if [[ ! -e "$PROJECT/.horizon/sdk/api" ]]; then
    ln -s "$(realpath "$HORIZON_HOME")" "$PROJECT/.horizon/sdk/api"
fi
if [[ $(realpath "$PROJECT/.horizon/sdk/api") != $(realpath "$HORIZON_HOME") ]]; then
    echo "$PROJECT/.horizon/sdk/api points to the wrong Horizon checkout" >&2
    exit 1
fi

export CARGO_TARGET_DIR="$PROJECT/target"

CHECK=$(mktemp -d /tmp/zmusic-horizon-check.XXXXXX)
cp "$PROJECT/Cargo.toml" "$PROJECT/Cargo.lock" "$CHECK/"
cp -a "$PROJECT/src" "$CHECK/"
mkdir -p "$CHECK/.horizon/sdk"
ln -s "$(realpath "$HORIZON_HOME")" "$CHECK/.horizon/sdk/api"
(
    cd "$CHECK"
    cargo test --lib
)

cargo build --manifest-path "$HORIZON_HOME/Cargo.toml" -p cargo-horizon-mod --locked

mkdir -p "$HORIZON_HOME/target"
STAGE=$(mktemp -d "$HORIZON_HOME/target/zmusic-package.XXXXXX")
cp "$PROJECT/Cargo.toml" "$PROJECT/Cargo.lock" "$PROJECT/horizon-mod.toml" "$PROJECT/LICENSE" "$STAGE/"
cp -a "$PROJECT/src" "$STAGE/"
mkdir -p "$STAGE/.horizon/sdk"
ln -s "$(realpath "$HORIZON_HOME")" "$STAGE/.horizon/sdk/api"

TOOL="$CARGO_TARGET_DIR/debug/cargo-horizon-mod"
(
    cd "$STAGE"
    "$TOOL" build
)
mkdir -p "$PROJECT/target/horizon-mod"
cp "$STAGE/target/horizon-mod/zmusic.hmod" "$PROJECT/target/horizon-mod/zmusic.hmod"
"$TOOL" check "$PROJECT/target/horizon-mod/zmusic.hmod"
printf 'Built %s\n' "$PROJECT/target/horizon-mod/zmusic.hmod"
