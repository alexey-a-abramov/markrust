#!/usr/bin/env bash
# Local release readiness checks (does not publish or tag).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> MarkRust release checks"
echo "    workspace: $ROOT"

if [[ "$(uname -s)" == "Linux" ]] && command -v apt-get >/dev/null 2>&1; then
  echo "==> Linux detected — ensure GPUI deps are installed (scripts/install-linux-deps.sh)"
fi

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy"
cargo clippy --locked --workspace --all-targets -- -D warnings

echo "==> cargo test"
cargo test --locked --workspace

echo "==> release build (markrust binary)"
cargo build --locked --release -p markrust

VERSION="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c '
import json, sys
for pkg in json.load(sys.stdin)["packages"]:
    if pkg["name"] == "markrust":
        print(pkg["version"])
        break
')"

echo "==> binary smoke test"
"./target/release/markrust" --version | grep -F "$VERSION"

echo ""
echo "Release checks passed for v${VERSION}."
echo "To publish: tag v${VERSION}, push tag, then update Homebrew SHA256 placeholders."
