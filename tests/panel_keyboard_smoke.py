"""Passive sidebars preserve terminal keys; explicitly focused editors still edit.

Run: xvfb-run -a python3 tests/panel_keyboard_smoke.py
"""
import os
from pathlib import Path
import re
import subprocess as sp
import tempfile
import time

root = Path(__file__).resolve().parents[1]
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'
work = Path(tempfile.mkdtemp(prefix='mypixels-panel-keys-'))
config = work / 'config.yml'
config.write_text('''wal_palette: ''
term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1280
default_height: 850
default_maximize: false
hide_single_tab: false
term_opacity: 0
''')
env = dict(os.environ, HOME=str(work), XDG_CONFIG_HOME=str(work / '.config'), MYPIXELS_CONFIG=str(config), RUST_LOG='info')
env.pop('WAYLAND_DISPLAY', None)
env.pop('MYPIXELS_KEYS', None)
binary = os.environ.get('MYPIXELS_BIN', str(root / 'target/debug/mypixels'))
log_path = work / 'app.log'
log = log_path.open('w')
app = sp.Popen([binary, '-s', 'python3', str(root / 'tests/pty_fixture.py'), str(work)], env=env, stdout=log, stderr=log)


def run(*args):
    return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=5).stdout


def wait(check, message):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        result = check()
        if result: return result
        assert app.poll() is None, log_path.read_text()
        time.sleep(.08)
    raise AssertionError(message + '\n' + log_path.read_text())


def key(value):
    run('xdotool', 'key', value)
    time.sleep(.12)


def click(x, y):
    run('xdotool', 'mousemove', '--window', wid, str(round(x)), str(round(y)), 'click', '1')
    time.sleep(.25)


def terminal_keys():
    global expected
    for value in ['Tab', 'Tab', 'Tab', 'a', 'Return', 'Escape', 'Down']:
        key(value)
    expected += b'\t\t\ta\r\x1b\x1b[B'
    wait(lambda: capture.read_bytes() == expected, f'terminal input stolen by sidebar: {capture.read_bytes()!r}')


try:
    def windows():
        return sp.run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)], env=env, stdout=sp.PIPE).stdout.decode().splitlines()
    wid = wait(windows, 'window missing')[-1]
    capture = wait(lambda: next(work.glob('input-*'), None), 'PTY missing')
    run('xdotool', 'windowfocus', '--sync', wid)
    time.sleep(.3)
    geometry = run('xdotool', 'getwindowgeometry', '--shell', wid).decode()
    width = int(re.search(r'WIDTH=(\d+)', geometry).group(1))
    strip = float(re.search(r'strip = ([0-9.]+)px', log_path.read_text()).group(1))
    expected = b''
    for panel in ['history', 'settings']:
        key('alt+shift+h' if panel == 'history' else 'alt+shift+c')
        time.sleep(.3)
        terminal_keys()
        # Clicking a filter or a settings page must not give the sidebar Tab.
        click(width - 330, strip + 83) if panel == 'history' else click(width - 448 + 156, strip + 105)
        terminal_keys()
        # App bindings still work while a sidebar is open.
        key('ctrl+shift+t')
        wait(lambda: len(list(work.glob('input-*'))) == (2 if panel == 'history' else 3), 'new-tab binding stopped working')
        key('alt+1')
        if panel == 'history':
            click(width - 64, strip + 34)
            run('import', '-window', wid, str(work / 'search-focused.png'))
            run('xdotool', 'type', 'needle')
            time.sleep(.2)
            assert capture.read_bytes() == expected, 'history search leaked into terminal'
            key('Escape')  # Leave the editor; history stays open.
        else:
            click(width - 448 + 235, strip + 244)
            key('ctrl+a')
            run('xdotool', 'type', '1234')
            key('Return')
            assert capture.read_bytes() == expected, 'settings editor leaked into terminal'
        terminal_keys()
        run('import', '-window', wid, str(work / f'{panel}.png'))
    # A global panel shortcut works even from a focused settings editor.
    click(width - 448 + 235, strip + 244)
    key('alt+shift+h')
    terminal_keys()
    assert all(not p.read_bytes() for p in work.glob('input-*') if p != capture), 'input reached an inactive tab'
    print('PASS: terminal Tab, typing, Enter, Escape and arrows with both sidebars; clicked controls, explicit editors, global shortcuts; artifacts:', work, flush=True)
finally:
    app.terminate()
    try: app.wait(timeout=3)
    except sp.TimeoutExpired:
        app.kill()
        app.wait()
    log.close()
