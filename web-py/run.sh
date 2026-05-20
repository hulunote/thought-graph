#!/usr/bin/env bash
# ThoughtGraph Python web edition launcher for Linux / macOS.
set -e
cd "$(dirname "$0")"
exec python3 server.py "$@"
