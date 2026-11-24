#!/bin/sh
# Wait (up to 10 s) for a running SupvanPrinter to finish quitting, so a
# reinstall followed by `open` does not race the old instance going away.
n=0
while pgrep -x SupvanPrinter >/dev/null && [ "$n" -lt 50 ]; do
    sleep 0.2
    n=$((n + 1))
done
