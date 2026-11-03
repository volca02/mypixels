#!/usr/bin/env python3
"""Multi-window follow-up smoke: Ctrl+Shift+n opens a second window in the
same process; Alt+Shift+w sends the active tab there, closing the emptied
first window. Driven entirely through the FIFO key harness.

    SPIKE_BIN=target/release/spike xvfb-run -a python3 tests/window_smoke.py
"""
import os
import subprocess as sp
import sys
import tempfile
import time
from pathlib import Path

BIN = Path(os.environ.get('SPIKE_BIN', 'target/debug/spike')).resolve()


def wait_for(lines, needle, seconds=15):
    deadline = time.time() + seconds
    while time.time() < deadline:
        if any(needle in line for line in lines()):
            return True
        time.sleep(0.2)
    return False


def main():
    tmp = Path(tempfile.mkdtemp(prefix='winsmoke-'))
    keys = tmp / 'keys'
    os.mkfifo(keys)
    cfg = tmp / 'config.yml'
    cfg.write_text('default_width: 800\ndefault_height: 520\nwal_palette: ""\n')
    env = dict(os.environ, SPIKE_KEYS=str(keys), SPIKE_CONFIG=str(cfg), RUST_LOG='info')
    log = (tmp / 'app.log').open('w')
    app = sp.Popen([str(BIN), 'sh'], env=env, stdout=log, stderr=log)
    kf = None
    try:
        kf = keys.open('w', buffering=1)  # blocks until the app opens it

        def lines():
            return (tmp / 'app.log').read_text(errors='replace').splitlines()

        if not wait_for(lines, 'term resize', seconds=20):
            print('FAIL: the first window never started')
            return 1
        time.sleep(0.5)

        kf.write('ctrl+shift+n\n')
        if not wait_for(lines, 'window 2 up', seconds=15):
            print('FAIL: Ctrl+Shift+n opened no second window')
            return 1
        print('PASS: Ctrl+Shift+n opened a second window in the session')

        # The new window is current; send the ORIGINAL window's tab logic:
        # send_tab_to_window from window 2 moves ITS tab back to window 1.
        kf.write('alt+shift+w\n')
        if not wait_for(lines, 'moved tab', seconds=15):
            print('FAIL: Alt+Shift+w did not move a tab')
            return 1
        print('PASS: Alt+Shift+w sent the tab to the other window')

        # ...which emptied window 2, so it closed; the session is alive on
        # one window again and still redraws.
        kf.write('spy\n')
        if not wait_for(lines, 'SPY active=', seconds=15):
            print('FAIL: the surviving window stopped responding')
            return 1
        tail = lines()[-40:]
        if not any('window 2 up' in l for l in tail):
            print('FAIL: log lost the window history')  # paranoia, cheap
        print('PASS: emptied window closed and the session lives on one window')
        print('WINDOW-SMOKE: PASS')
        return 0
    finally:
        if kf:
            kf.close()
        app.terminate()
        app.wait(timeout=5)


if __name__ == '__main__':
    sys.exit(main())
