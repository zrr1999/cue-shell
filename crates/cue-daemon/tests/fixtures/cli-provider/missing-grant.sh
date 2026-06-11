#!/bin/sh
set -eu

case "${1:-}" in
  probe)
    printf '%s\n' '{"units":[]}'
    ;;
  reserve)
    cat >/dev/null
    printf '%s\n' '{"ok":true}'
    ;;
  release)
    cat >/dev/null
    printf '%s\n' '{}'
    ;;
  *)
    echo "usage: $0 probe|reserve|release" >&2
    exit 64
    ;;
esac
