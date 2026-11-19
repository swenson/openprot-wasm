#!/bin/bash
# Licensed under the Apache-2.0 license
#
# Build the site in www/:
#   * the firmware of the scenarios in scenarios.json, built with Bazel in an
#     OpenPRoT checkout, into www/images/ and www/scenarios.json;
#   * the emulator (emulator/) as a WASM module, into www/pkg/;
#   * www/build-info.json, which names the emulator's sources.
#
# Environment:
#   OPENPROT_DIR    Build the firmware in this OpenPRoT checkout, at whatever
#                   it has checked out. By default the revision pinned in
#                   scenarios.json is checked out under OPENPROT_CACHE_DIR.
#   OPENPROT_CACHE_DIR  Where that checkout is kept (default:
#                   ~/.cache/openprot-wasm). Must be outside this repository.
#   SCENARIOS       Ids of the scenarios to build, separated by spaces
#                   (default: all of scenarios.json).
#   SKIP_FIRMWARE=1 Keep the firmware already in www/ and build the emulator
#                   only.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

config() {
    python3 -c 'import json, sys; print(json.load(open("scenarios.json"))["openprot"][sys.argv[1]])' "$1"
}

# Check out OpenPRoT at the pinned revision and print its directory.
openprot_checkout() {
    local url rev cache dir
    url="$(config repo)"
    rev="$(config rev)"
    cache="${OPENPROT_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/openprot-wasm}"
    case "$cache/" in
        "$SCRIPT_DIR"/*)
            echo "ERROR: OPENPROT_CACHE_DIR must be outside $SCRIPT_DIR" >&2
            return 1
            ;;
    esac
    dir="$cache/openprot"
    if [ ! -d "$dir/.git" ]; then
        echo "Cloning $url into $dir..." >&2
        mkdir -p "$cache"
        git clone --quiet --no-checkout "$url" "$dir" >&2
    fi
    if ! git -C "$dir" cat-file -e "$rev^{commit}" 2>/dev/null; then
        echo "Fetching OpenPRoT $rev..." >&2
        git -C "$dir" fetch --quiet origin "$rev" >&2
    fi
    git -C "$dir" checkout --quiet --force --detach "$rev" >&2
    echo "$dir"
}

if [ "${SKIP_FIRMWARE:-0}" != "1" ]; then
    if [ -n "${OPENPROT_DIR:-}" ]; then
        openprot="$(cd "$OPENPROT_DIR" && pwd)"
    else
        openprot="$(openprot_checkout)"
    fi
    echo "=== Firmware (OpenPRoT at $openprot) ==="
    # shellcheck disable=SC2086
    ./tools/build-scenarios.py --openprot "$openprot" --www www ${SCENARIOS:+--only $SCENARIOS}
elif [ ! -f www/scenarios.json ]; then
    echo "ERROR: SKIP_FIRMWARE=1, but www/scenarios.json has not been built" >&2
    exit 1
fi

echo ""
echo "=== Emulator ==="
if ! command -v wasm-pack &>/dev/null; then
    echo "wasm-pack not found. Installing..."
    cargo install wasm-pack --locked
fi
(cd emulator && rustup toolchain install >/dev/null)
(cd emulator && wasm-pack build --target web --release --no-typescript)
mkdir -p www/pkg
cp emulator/pkg/openprot_wasm.js emulator/pkg/openprot_wasm_bg.wasm www/pkg/

# The revisions the emulator was built from, for the page's footer.
python3 - <<'PYEOF'
import json, re

manifest = open("emulator/Cargo.toml").read()

def pin(repo):
    url, rev = re.search(r'git = "(https://github.com/[^"]+/%s)", rev = "([0-9a-f]+)"' % repo, manifest).groups()
    return {"repo": url, "rev": rev}

info = {"caliptra_sw": pin("caliptra-sw"), "caliptra_mcu_sw": pin("caliptra-mcu-sw")}
with open("www/build-info.json", "w") as f:
    json.dump(info, f, indent=2)
    f.write("\n")
print("Wrote www/build-info.json")
PYEOF

echo ""
echo "Build complete. To run:"
echo "  cd $SCRIPT_DIR/www && python3 -m http.server 8080"
echo "  Then open http://localhost:8080"
