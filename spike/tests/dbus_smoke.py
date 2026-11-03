#!/usr/bin/env python3
"""Daemon-mode smoke: the second mypixels hands its command to the running
session over D-Bus instead of opening a second window; --standalone hosts
nothing and joins nothing.

Run inside a private bus and display:

    dbus-run-session -- xvfb-run -a python3 tests/dbus_smoke.py

"""
import os
import subprocess as sp
import sys
import tempfile
import time
from pathlib import Path

BIN = Path(os.environ.get('SPIKE_BIN', 'target/debug/spike')).resolve()


def home(tag, keys):
    h = Path(tempfile.mkdtemp(prefix=f'dbus-{tag}-'))
    os.mkfifo(h / keys)
    cfg = h / '.config/mypixels'
    cfg.mkdir(parents=True)
    (cfg / 'config.yml').write_text(
        'default_width: 800\ndefault_height: 480\nwal_palette: ""\n')
    return h


def env_for(h, keys):
    env = dict(os.environ, HOME=str(h), SPIKE_KEYS=str(h / keys),
               HISTFILE=str(h / '.history'), RUST_LOG='info')
    env.pop('SPIKE_CONFIG', None)
    return env


def wait_for(path, needle, seconds=15):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            if needle in path.read_text():
                return True
        except FileNotFoundError:
            pass
        time.sleep(0.1)
    return False


def main():
    if not os.environ.get('DBUS_SESSION_BUS_ADDRESS'):
        print('DBUS-SMOKE: SKIP (no session bus; run under dbus-run-session)')
        return 0
    if not os.environ.get('DISPLAY') and not os.environ.get('WAYLAND_DISPLAY'):
        print('DBUS-SMOKE: SKIP (no display; run under xvfb-run)')
        return 0

    ha = home('session', 'keys')
    log_a = (ha / 'app.log').open('w')
    a = sp.Popen([str(BIN), 'sh'], env=env_for(ha, 'keys'),
                 stdout=log_a, stderr=log_a)
    try:
        # 1. The first instance registers the session.
        if not wait_for(ha / 'app.log', 'session registered'):
            print('FAIL: session A never registered its bus name')
            return 1
        print('PASS: first instance registered the display session')

        # 2. A plain second run forwards itself and exits quickly.
        hb = home('client', 'keys')
        t0 = time.time()
        # The command touches a marker file on arrival: relative path, so it
        # lands wherever the session actually started the tab. cwd correctness
        # is proved in bytes, not in titles.
        b = sp.run([str(BIN), 'sh', '-c', 'touch daemon-marker; exec sh'],
                   env=env_for(hb, 'keys'), cwd=str(hb),
                   capture_output=True, timeout=10)
        if b.returncode != 0 or time.time() - t0 > 8:
            print(f'FAIL: joining client exited {b.returncode} after '
                  f'{time.time() - t0:.1f}s: {b.stderr.decode()[:200]}')
            return 1
        print('PASS: second run exited %d after %.1fs' % (b.returncode, time.time() - t0))

        # 3. The session opened the tab it was handed -- in the *client's*
        # directory, which is what makes the workflow worth having.
        if not wait_for(ha / 'app.log', 'opened a remote tab'):
            print('FAIL: session A never reported the remote tab')
            return 1
        if not (hb / 'daemon-marker').exists():
            deadline = time.time() + 5
            while time.time() < deadline and not (hb / 'daemon-marker').exists():
                time.sleep(0.1)
            if not (hb / 'daemon-marker').exists():
                print('FAIL: remote tab did not run in the client\'s directory')
                return 1
        print('PASS: remote tab ran in the client cwd')

        # 4. `mypixels -n` asks the LIVE session for a window: same process,
        # second window, tab running the command in the caller's directory.
        hd = home('window2', 'keys')
        before = (ha / 'app.log').stat().st_size
        d = sp.run([str(BIN), '-n', 'sh', '-c', 'touch window-marker; exec sh'],
                   env=env_for(hd, 'keys'), cwd=str(hd),
                   capture_output=True, timeout=10)
        if d.returncode != 0:
            print(f'FAIL: -n client exited {d.returncode}: {d.stderr.decode()[:200]}')
            return 1
        if not wait_for(ha / 'app.log', 'opened a remote window'):
            print('FAIL: session A never opened the requested window')
            return 1
        deadline = time.time() + 10
        while time.time() < deadline and not (hd / 'window-marker').exists():
            time.sleep(0.2)
        if not (hd / 'window-marker').exists():
            print('FAIL: the new window ran its tab somewhere else than asked')
            return 1
        print('PASS: -n opened a second window inside the session')

        # 5. --standalone hosts nothing: no registration, still a terminal.
        hc = home('solo', 'keys')
        log_c = (hc / 'app.log').open('w')
        c = sp.Popen([str(BIN), '-s', 'sh'], env=env_for(hc, 'keys'),
                     stdout=log_c, stderr=log_c)
        try:
            with (hc / 'keys').open('w') as f:
                f.write('spy\n')
            if not wait_for(hc / 'app.log', 'SPY active=', seconds=15):
                print('FAIL: --standalone instance never started')
                return 1
            time.sleep(0.5)  # let a registration slip in if it were coming
            if 'session registered' in (hc / 'app.log').read_text():
                print('FAIL: --standalone instance registered a bus name')
                return 1
            print('PASS: --standalone instance runs without a bus name')
        finally:
            c.terminate()
            c.wait(timeout=5)

        print('DBUS-SMOKE: PASS')
        return 0
    finally:
        a.terminate()
        a.wait(timeout=5)


if __name__ == '__main__':
    sys.exit(main())
