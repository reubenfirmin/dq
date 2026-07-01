#!/bin/bash
set -e

BIN_DIR="$HOME/.local/bin"

if [ ! -f ./dq ]; then
	echo "./dq not found. Run ./build.sh first." >&2
	exit 1
fi

mkdir -p "$BIN_DIR"
cp ./dq "$BIN_DIR/dq"
echo "Installed dq to $BIN_DIR/dq"

case ":$PATH:" in
	*":$BIN_DIR:"*) ;;
	*) echo "Note: $BIN_DIR is not on your PATH. Add it, e.g. export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
