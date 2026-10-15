#!/usr/bin/env bash
# Runs every scripts/ test: the pure-framing Perl unit tests, then the
# cast.sh integration tests against fake servers. See README's "Casting from
# a computer" section.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

perl "$SCRIPT_DIR/test_fcast_client.pl"
bash "$SCRIPT_DIR/test_cast_sh.sh"
