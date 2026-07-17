#!/bin/bash
set -e
cd "$(dirname "$0")"
exec bash ./scripts/install.sh "$@"
