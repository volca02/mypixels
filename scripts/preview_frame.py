#!/usr/bin/env python3
"""Paint the visible terminal area so the switcher's preview extent is obvious.

Fills the screen with a flat coloured background and draws a one-character
frame around all four edges, then holds until interrupted. Run it in a tab
and hold Alt: the preview readback samples everything below the tab strip,
so the frame should sit exactly on the thumbnail's edges if the sampling
extent is the full terminal area -- and anything blank you see in the
preview was blank in the terminal all along.
"""
import fcntl
import struct
import sys
import termios
import time


def screen_size():
    fd = sys.stdout.fileno()
    rows, cols, _, _ = struct.unpack("hhhh", fcntl.ioctl(fd, termios.TIOCGWINSZ, b"\x00" * 8))
    return max(rows, 2), max(cols, 4)


def main():
    rows, cols = screen_size()

    # Dark teal background, bright yellow frame (truecolor; tweak freely).
    bg = "\x1b[48;2;18;46;46m"
    fg = "\x1b[38;2;255;214;64m"
    reset = "\x1b[0m"

    top = fg + "█" * cols
    mid = fg + "█" + bg + " " * (cols - 2) + fg + "█"
    lines = [top] + [mid] * (rows - 2) + [top]

    sys.stdout.write("\x1b[2J\x1b[H")  # clear + home, without scrolling
    sys.stdout.write("\r\n".join(lines))  # exactly `rows` lines, no trailing LF
    sys.stdout.write(reset)
    sys.stdout.flush()

    try:
        time.sleep(3600)  # hold; the grid keeps showing the frame after exit
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write(reset + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
