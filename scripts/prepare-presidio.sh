#!/usr/bin/env bash
#
# Build the BUNDLED Presidio runtime so the app ships with it (zero runtime download).
# Produces:
#   vendor/presidio-env/python — a SELF-CONTAINED relocatable standalone Python (python-build-
#                                standalone) with presidio + spaCy + the small model installed
#   vendor/presidio-src/...     — the minimal Flask entry points (app.py) for each service
# electron-builder then ships these as extraResources → resourcesPath/presidio-env + /presidio,
# which src/main/presidio.js auto-detects (bundledRoot / sourceDir).
#
# IMPORTANT: Python + the venv are PLATFORM-SPECIFIC. Run this on each target platform's build
# (macOS / Windows / Linux) so each installer carries its own native runtime. Requires `uv`.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$HERE/vendor/presidio-env"
SRCOUT="$HERE/vendor/presidio-src"
PYVER="${PRESIDIO_PYTHON:-3.12}"
MODEL="https://github.com/explosion/spacy-models/releases/download/en_core_web_sm-3.8.0/en_core_web_sm-3.8.0-py3-none-any.whl"

command -v uv >/dev/null 2>&1 || { echo "ERROR: uv not found on PATH"; exit 1; }
[ -f "$SRC/presidio-analyzer/app.py" ] || { echo "ERROR: presidio source not at $SRC (set PRESIDIO_SRC)"; exit 1; }

echo "==> [1/3] copy the SELF-CONTAINED standalone Python ($PYVER)"
# A uv venv only symlinks to the host's Python (breaks on other machines). Instead bundle the full
# relocatable python-build-standalone install and install Presidio into ITS OWN site-packages.
rm -rf "$OUT" "$SRCOUT"
mkdir -p "$OUT" "$SRCOUT"
uv python install "$PYVER" >/dev/null 2>&1 || true
PYBIN="$(uv python find "$PYVER")"
[ -x "$PYBIN" ] || { echo "ERROR: could not locate a uv standalone python $PYVER"; exit 1; }
PYROOT="$(cd "$(dirname "$PYBIN")/.." && pwd)"
case "$PYROOT" in *cpython*|*uv/python*) : ;; *) echo "ERROR: $PYROOT is not a relocatable uv standalone python"; exit 1;; esac
cp -R "$PYROOT" "$OUT/python"

echo "==> [2/3] install presidio (server extras) + spaCy small model into the bundled Python"
uv pip install --python "$OUT/python/bin/python3.12" --break-system-packages \
  "$SRC/presidio-analyzer[server]" "$SRC/presidio-anonymizer[server]" "$MODEL"

echo "==> [3/3] copy the Flask entry points (+ the logging.ini each app.py reads at startup)"
for svc in presidio-analyzer presidio-anonymizer; do
  mkdir -p "$SRCOUT/$svc"
  cp "$SRC/$svc/app.py" "$SRCOUT/$svc/app.py"
  cp "$SRC/$svc/logging.ini" "$SRCOUT/$svc/logging.ini"
done

echo "done → $OUT  +  $SRCOUT"
