#!/bin/bash
set -e

echo "Building dq (release)"

if cargo build --release; then
	cp target/release/dq ./dq
	echo "Built ./dq"
else
	echo "Build failed!" >&2
	exit 1
fi
