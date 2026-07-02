#!/bin/bash
set -e

echo "Building dq/pq (release)"

if cargo build --release; then
	cp target/release/dq ./dq
	echo "Built ./dq"
	cp target/release/pq ./pq 2>/dev/null && echo "Built ./pq" || true
else
	echo "Build failed!" >&2
	exit 1
fi
