#!/usr/bin/env bash
# build, exercise and bundle a candidate; never tag, publish or install into a running server.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
: "${PGRX_PG_CONFIG_PATH:?use a private PostgreSQL 18.6 installation}"
: "${PIN_PG_SOURCE:?use the pinned PostgreSQL source checkout}"
[[ $(id -u) != 0 ]] || { echo 'run packaging as a non-root user' >&2; exit 1; }
[[ $("$PGRX_PG_CONFIG_PATH" --version) == 'PostgreSQL 18.6' ]]
[[ $(rustc --version) == 'rustc 1.98.1 '* ]]
[[ -z ${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-} ]] || { echo 'custom Rust flags require separate qualification' >&2; exit 1; }
git diff --exit-code
git diff --cached --exit-code
[[ -z $(git ls-files --others --exclude-standard) ]] || { echo 'candidate checkout contains untracked files' >&2; exit 1; }
export PIN_BUILD_REVISION=$(git rev-parse HEAD)
export PIN_RELEASE_PACKAGE=1
export PATH="$("$PGRX_PG_CONFIG_PATH" --bindir):$PATH"
work=$(mktemp -d "${TMPDIR:-/tmp}/pin-package.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/stage/"{lib,extension,metadata}
# the exact rejection must come from our build guard, not an unrelated failure.
if cargo check --locked -p pin-pg --features test-hooks > "$work/rejected-build.log" 2>&1; then
    echo 'deployment build accepted test-hooks' >&2
    exit 1
fi
grep -q 'deployment builds require a source commit and forbid test-hooks' "$work/rejected-build.log"
mkdir -p .artifacts/g8
cp "$work/rejected-build.log" .artifacts/g8/rejected-build.log
# pgrx remains the sole SQL generator; lock drift rejects the candidate.
(cd crates/pin-pg && cargo pgrx install --release --pg-config "$PGRX_PG_CONFIG_PATH")
git diff --exit-code -- Cargo.lock
bash tools/g8_qualification.sh
install -m 644 "$("$PGRX_PG_CONFIG_PATH" --pkglibdir)/pin.so" "$work/stage/lib/pin.so"
for name in pin.control pin--0.0.0.sql; do
    install -m 644 "$("$PGRX_PG_CONFIG_PATH" --sharedir)/extension/$name" "$work/stage/extension/$name"
done
install -m 644 Cargo.lock "$work/stage/metadata/Cargo.lock"
# inspect symbols as well as generated SQL and the executed build-profile function.
nm --dynamic --defined-only "$work/stage/lib/pin.so" > "$work/symbols"
if grep -Eq '\b(g0_[[:alnum:]_]*|g2_inject[[:alnum:]_]*|pin_test_[[:alnum:]_]*)\b' "$work/symbols"; then
    echo 'test symbols found in deployment library' >&2
    exit 1
fi
cargo metadata --locked --format-version 1 --manifest-path crates/pin-pg/Cargo.toml \
    --no-default-features --features pg18 > "$work/metadata.json"
python3 - "$root" "$work" "$PIN_BUILD_REVISION" <<'PY'
import json
from pathlib import Path
import sys
sys.path.insert(0, sys.argv[1])
from tools.g8_package import encoded, validate_metadata
root, work, revision = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
metadata = json.loads((work / 'metadata.json').read_bytes())
# preserve the resolved dependency/feature graph, not build-machine paths.
def identity(value):
    return value.replace(root.as_uri(), 'workspace:')
deps = {
    'packages': sorted(({
        key: identity(package[key]) if isinstance(package.get(key), str) else package.get(key)
        for key in ('id', 'name', 'version', 'source', 'license')
    } for package in metadata['packages']), key=lambda package: package['id']),
    'resolve': {'nodes': sorted(({
        'id': identity(node['id']),
        'features': sorted(node['features']),
        'dependencies': sorted(identity(dep) for dep in node['dependencies']),
    } for node in metadata['resolve']['nodes']), key=lambda node: node['id'])},
}
build = {
    'schema': 1, 'status': 'qualification-only', 'profile': 'normal',
    'postgres': '18.6', 'rust': '1.98.1', 'pgrx': '0.19.2',
    'target': 'x86_64-unknown-linux-gnu', 'sql_version': '0.0.0', 'commit': revision,
    'build_command': 'cargo pgrx install --release',
    'standby_index_reads': False,
    'tests': ['001_operational.pl', '002_logical.pl'],
    'independent_release_review': 'pending',
}
validate_metadata(build, deps)
(work / 'stage/metadata/build.json').write_bytes(encoded(build))
(work / 'stage/metadata/dependencies.json').write_bytes(encoded(deps))
PY
mkdir -p .artifacts/g8-release
output="$root/.artifacts/g8-release/pin-0.0.0-pg18.6-${PIN_BUILD_REVISION}.tar.gz"
python3 tools/g8_package.py assemble "$work/stage" "$output"
python3 tools/g8_package.py assemble "$work/stage" "$work/repeated.tar.gz"
cmp "$output" "$work/repeated.tar.gz"
(cd .artifacts/g8-release && sha256sum "$(basename "$output")" > "$(basename "$output").sha256")
printf '%s\n' "Candidate bundle: $output" 'No production qualification or performance parity is implied.'
