#!/usr/bin/env bash
# Runs the Sijo benchmark suite (see bench/run.py --help).
#   bench/run.sh                    # -O2 (sijoc's flags), 3 repetitions, writes bench/RESULTS.md tables
#   bench/run.sh --quick            # smoke test (< 1 minute)
#   bench/run.sh --native           # also -O3 -march=native (what bench/RESULTS.md was produced with)
# Needs: Rust/cargo, gcc/g++ with OpenMP, python3; optional: dotnet (C# variant; or pass --dotnet PATH).
set -euo pipefail
exec python3 "$(dirname "$0")/run.py" "$@"
