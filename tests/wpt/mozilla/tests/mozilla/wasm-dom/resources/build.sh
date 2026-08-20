#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Reassembles the .wasm fixtures from their .wat sources.
#
# Both are checked in on purpose. The .wat is the source of truth and the only reviewable
# form; the .wasm is what the tests actually load, because WPT cannot assemble text format at
# runtime. There is in-tree precedent at tests/wpt/tests/wasm/incrementer.wasm.
#
# wabt is deliberately NOT a build dependency -- running this is a manual step after editing a
# .wat. Re-run it and check that `git diff` is empty to confirm the two have not drifted.

set -o errexit
set -o nounset
set -o pipefail

cd "$(dirname "${0}")"

if ! command -v wat2wasm >/dev/null; then
  echo "wat2wasm not found. Install wabt (brew install wabt / apt install wabt)." >&2
  exit 1
fi

for source in *.wat; do
  wat2wasm "${source}" -o "${source%.wat}.wasm"
  echo "assembled ${source%.wat}.wasm from ${source}"
done
