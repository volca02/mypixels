#!/usr/bin/env python3
"""Full-screen redraw stress test for terminals.

Mimics an agentic TUI: alternate screen, full grid repaint every frame,
colored text, cursor movement. Writes stats to the file given in argv.

Usage: stress.py <seconds> <fps> <statfile>
"""
import os
import sys
import time

def main():
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 12.0
    fps = int(sys.argv[2]) if len(sys.argv) > 2 else 30
    statfile = sys.argv[3] if len(sys.argv) > 3 else "/tmp/stress.stat"

    cols = 100
    rows = 30

    out = sys.stdout

    # Alternate screen, hide cursor
    out.write("\x1b[?1049h\x1b[?25l")
    out.flush()

    # 50 cell pairs, so each row = 100 chars
    cells = ["%02d" % i for i in range(50)]

    frames = 0
    bytes_written = 0
    t0 = time.monotonic()
    while True:
        now = time.monotonic()
        if now - t0 >= duration:
            break
        frame_t = int(now * 10)
        parts = ["\x1b[H"]
        for r in range(rows):
            color = 232 + ((r + frame_t) % 16)
            off = (r * 7 + frame_t * 3) % 50
            parts.append(f"\x1b[38;5;{color}m")
            parts.append("".join(cells[(off + c) % 50] for c in range(50)))
            if r < rows - 1:
                parts.append("\r\n")
        data = "".join(parts)
        out.write(data)
        out.flush()
        bytes_written += len(data)
        frames += 1
        # Pace to target fps; if we fall behind (pty backpressure), we just run late
        target = t0 + frames / fps
        delay = target - time.monotonic()
        if delay > 0:
            time.sleep(delay)

    wall = time.monotonic() - t0
    out.write("\x1b[?25h\x1b[0m\x1b[?1049l")
    out.flush()
    with open(statfile, "w") as f:
        f.write(f"frames={frames} delivered_fps={frames / wall:.1f} "
                f"target_fps={fps} bytes={bytes_written} "
                f"mbps={bytes_written / wall / 1e6:.1f}\n")

if __name__ == "__main__":
    main()
