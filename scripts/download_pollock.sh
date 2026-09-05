#!/usr/bin/env bash
# Fetch the Pollock data-loading benchmark (Vitagliano et al., PVLDB 16, 2023).
#
# 2,290 CSV files, each one clean file with a single isolated deviation from
# RFC 4180 — a different delimiter, a multi-row header, a preamble, one row
# with an extra separator, a non-standard escape — plus the clean content each
# one should load back to, and the metric that scores the result at header,
# record and cell level.
#
# ~2.8 GB, gitignored, and nothing in CI touches it. `scripts/run_pollock.py`
# is what reads it; see `gap_reports/pollock_summary.md` for the last result.
set -euo pipefail

DEST="${1:-pollock}"
VENV="${DEST}-venv"

if [ -d "$DEST/.git" ]; then
    echo "$DEST already cloned; pulling"
    git -C "$DEST" pull --ff-only
else
    git clone --depth 1 https://github.com/HPI-Information-Systems/Pollock.git "$DEST"
fi

# The scorer runs Pollock's own metrics module rather than a reimplementation,
# so its dependencies have to exist somewhere. A venv beside the clone keeps
# them out of the system python and out of this repo's build.
if [ ! -x "$VENV/bin/python" ]; then
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install -q chardet numpy joblib multiset python-dateutil price-parser
fi

cat <<EOF

Ready. Score tdy against it with:

  cargo build --release
  scripts/run_pollock.py --pollock $DEST --venv $VENV

Writes gap_reports/pollock_summary.md (gitignored, like every gap report).
EOF
