#!/usr/bin/env python3
"""Insert `true,` as pty_enabled arg in all spawn_chain test calls."""

path = "crates/cue-daemon/src/actor/scheduler.rs"
with open(path) as f:
    lines = f.readlines()

# Fix pattern: in test functions, spawn_chain calls have:
# ..., false,
#             0,
# Need: ..., false, true,
#             0,

i = 0
while i < len(lines):
    line = lines[i]
    # Check if this line has `false,` and the next has `0,` (inside spawn_chain)
    if line.rstrip().endswith('false,') and i + 1 < len(lines):
        next_line = lines[i + 1]
        if next_line.strip().startswith('0,') or next_line.strip().startswith('retry_max,'):
            # This is likely a spawn_chain call site - insert true after false
            lines[i] = line.rstrip()[:-1] + ' true,\n'
    i += 1

with open(path, 'w') as f:
    f.writelines(lines)

print("Done fixing test callers")
