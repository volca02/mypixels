#!/usr/bin/env python3
"""Drive a mypixels instance through its dedicated test channel.

The app reads one directive per line from the $MYPIXELS_KEYS FIFO:
key specs ("ctrl+c", "alt+shift+t"), state dumps ("spy"), and raw pty
writes ("line:ls -la" runs `ls -la` + Enter in the active tab). This
harness creates the FIFO, spawns a standalone instance, feeds it, and
screenshots - no uinput, no focus stealing, safe to run while the human
is at the keyboard.

  scripts/live_harness.py --run 'broot ~/Pictures' --send 'line:~'
  scripts/live_harness.py --run bash --send 'line:scripts/png_passthrough.sh x.png' --shot s.png
  # directives: line:<cmd> text:<bytes> spy shot switcher alt_up, or any
  # key spec; --send is repeatable, --sleep between sends.
"""
import argparse
import os
import random
import signal
import subprocess
import sys
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", default="bash", help="command for the first tab")
    ap.add_argument("--send", action="append", default=[],
                    help="FIFO directive (repeatable)")
    ap.add_argument("--sleep", type=float, default=1.0, help="after each send")
    ap.add_argument("--wait", type=float, default=2.5, help="after spawn")
    ap.add_argument("--settle", type=float, default=1.5, help="after all sends")
    ap.add_argument("--dump", default=None, help="MYPIXELS_APC_DUMP path")
    ap.add_argument("--spy", action="store_true", help="request a state dump")
    ap.add_argument("--shot", default=None)
    ap.add_argument("--crop", default=None, help="x,y,w,h region of the screen")
    ap.add_argument("--grid", default="150x46")
    ap.add_argument("--focus", action="store_true",
                    help="also focus the window (only when realism needs it)")
    ap.add_argument("--kill", action="store_true")
    ap.add_argument("--no-standalone", action="store_true")
    a = ap.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    fifo = f"/tmp/mypixels-keys.{os.getpid()}.{random.randrange(1 << 30):08x}"
    if os.path.exists(fifo):
        os.unlink(fifo)
    os.mkfifo(fifo)

    env = dict(os.environ, MYPIXELS_GRID=a.grid, MYPIXELS_KEYS=fifo)
    if a.dump:
        env["MYPIXELS_APC_DUMP"] = a.dump
    cmd = [os.path.join(root, "target/release/mypixels"), "--quiet"]
    if not a.no_standalone:
        cmd.append("--standalone")
    cmd += ["--"] + a.run.split()
    p = subprocess.Popen(cmd, env=env, cwd=root,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"pid {p.pid} fifo {fifo}")
    try:
        # The reader opens the FIFO itself; opening for write blocks until
        # it exists, which doubles as startup synchronization.
        t0 = time.time()
        fh = None
        while time.time() - t0 < 20:
            if p.poll() is not None:
                sys.exit(f"instance exited early (rc={p.returncode})")
            try:
                # nonblocking until the reader shows up
                fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
                fh = os.fdopen(fd, "w")
                break
            except OSError:
                time.sleep(0.15)
        if fh is None:
            sys.exit("key FIFO never opened")
        print(f"channel up after {time.time() - t0:.1f}s")
        time.sleep(a.wait)

        def send(line):
            fh.write(line + "\n")
            fh.flush()
            time.sleep(a.sleep)

        for s in a.send:
            send(s)
        if a.spy:
            send("spy")
        time.sleep(a.settle)
        if a.shot:
            if a.crop:
                x, y, w, h = (int(v) for v in a.crop.split(","))
                subprocess.run(["grim", "-g", f"{x},{y} {w}x{h}", a.shot])
            else:
                subprocess.run(["grim", a.shot])
            print("shot", a.shot)
        if a.dump and os.path.exists(a.dump):
            d = open(a.dump, "rb").read()
            print("apc bytes", len(d), "frames", d.count(b"APCFRAME"))
    finally:
        if a.kill:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
        else:
            print(f"left running: {p.pid}")
        fh.close() if 'fh' in dir() else None
        os.unlink(fifo)


main()
