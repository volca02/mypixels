"""Real pixels, theme reload, OSC palette overrides, and the Alt-held switcher.

Run against a disposable X server: xvfb-run -a python3 tests/theme_smoke.py
Requires xdotool, ImageMagick import, and Pillow. Never uses the desktop HOME.
"""
import os
import pathlib
import re
import subprocess as sp
import tempfile
import time

from PIL import Image

root = pathlib.Path(__file__).resolve().parents[1]
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'
work = pathlib.Path(tempfile.mkdtemp(prefix='mypixels-themes-'))
theme = work / 'live.yml'
theme.write_text((root / 'themes/dracula.yml').read_text())
config = work / '.mypixels'
base = """wal_palette: ''
term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1000
default_height: 600
default_maximize: false
term_opacity: 0
hide_single_tab: false
"""
config.write_text(base + 'theme: live.yml\n')
fixture = work / 'fixture.py'
fixture.write_text(r'''
import os, pathlib, sys, tty
tty.setraw(0)
pathlib.Path(sys.argv[1], f'ready-{os.getpid()}').touch()
out = '\x1b]2;Theme workshop\x07mypixels  /  Color workshop\r\n'
for indexed in (False, True):
    for i in range(16):
        code = f'48;5;{i}' if indexed else str(40+i if i < 8 else 100+i-8)
        out += f'\x1b[{code}m    '
    out += '\x1b[0m\r\n'
out += '\r\n'
for i in range(16):
    code = 30+i if i < 8 else 90+i-8
    out += f'\x1b[{code}mAa  '
out += '\x1b[0m\r\n\r\n'
out += 'Normal  \x1b[1mBold\x1b[0m  \x1b[3mItalic\x1b[0m  Unicode: č λ 🦀\r\n'
out += 'drwxr-xr-x  12 volca volca 4096 Oct 31 08:00 src\r\n'
out += '-rw-r--r--   1 volca volca 2048 Oct 31 08:00 Cargo.toml\r\n'
out += '\r\nLive themes: terminal, tabs, selection, cursor, and switcher.\r\n'
os.write(1, out.encode())
while True:
    data = os.read(0, 65536)
    if b'r' in data:
        os.write(1, b'\x1b]4;1;#123456\x07\x1b]11;#101820\x07')
    if b's' in data:
        os.write(1, b'\x1b]104;1\x07\x1b]111\x07')
''')
env = dict(os.environ, HOME=str(work), MYPIXELS_CONFIG=str(config), RUST_LOG='info')
env.pop('WAYLAND_DISPLAY', None)
env.pop('MYPIXELS_KEYS', None)
log = (work / 'app.log').open('w')
app = sp.Popen([(os.environ.get('MYPIXELS_BIN') or os.environ.get('MYPIXELS_BIN') or str(root / 'target/debug/mypixels')), '/usr/bin/python3',
                str(fixture), str(work)], env=env, stdout=log, stderr=log)


def run(*args):
    return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=5).stdout


def wait(check, message):
    until = time.monotonic() + 8
    while time.monotonic() < until:
        if check():
            return
        assert app.poll() is None, (work / 'app.log').read_text()
        time.sleep(.08)
    raise AssertionError(message + '\n' + (work / 'app.log').read_text())


try:
    windows = []

    def find_window():
        result = sp.run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)],
                        env=env, stdout=sp.PIPE)
        windows[:] = result.stdout.decode().splitlines()
        return windows

    wait(find_window, 'window missing')
    wid = windows[-1]
    wait(lambda: list(work.glob('ready-*')), 'child missing')
    run('xdotool', 'windowfocus', '--sync', wid)
    metrics = re.search(r'cell = ([0-9.]+) x ([0-9.]+),.*strip = ([0-9.]+)px',
                        (work / 'app.log').read_text())
    cw, ch, strip = map(float, metrics.groups())

    def screenshot(name='probe'):
        path = work / (name + '.png')
        run('import', '-window', wid, str(path))
        with Image.open(path) as image:
            return image.convert('RGB')

    def background_is(color):
        image = screenshot()
        return image.getpixel((image.width - 8, image.height * 3 // 4)) == color

    def block(image, row, index):
        return image.getpixel((round((index * 4 + 2) * cw), round(strip + (row + .5) * ch)))

    def key(key):
        run('xdotool', 'key', key)

    wait(lambda: background_is((40, 42, 54)), 'Dracula background missing')
    wait(lambda: block(screenshot(), 1, 1) == (255, 85, 85), 'initial output missing')
    image = screenshot('dracula')
    assert block(image, 1, 1) == block(image, 2, 1) == (255, 85, 85)
    assert block(image, 1, 9) == block(image, 2, 9) == (255, 110, 110)

    key('r')
    wait(lambda: background_is((16, 24, 32)), 'OSC 11 background ignored')
    image = screenshot('osc')
    assert block(image, 1, 1) == block(image, 2, 1) == (18, 52, 86), 'OSC 4 indexed/named mismatch'
    key('s')
    wait(lambda: background_is((40, 42, 54)), 'OSC reset did not restore theme')

    key('ctrl+shift+t')
    wait(lambda: len(list(work.glob('ready-*'))) == 2, 'second tab missing')
    wait(lambda: block(screenshot(), 1, 1) == (255, 85, 85), 'second tab output missing')
    # Atomic replacement, like editors and pywal, must wake an idle terminal.
    replacement = work / 'replacement.yml'
    replacement.write_text((root / 'themes/catppuccin-latte.yml').read_text())
    replacement.replace(theme)
    wait(lambda: background_is((239, 241, 245)), 'idle theme file did not reload')
    image = screenshot('latte')
    assert block(image, 1, 1) == block(image, 2, 1) == (210, 15, 57)
    key('alt+Left')
    wait(lambda: background_is((239, 241, 245)), 'inactive tab retained the previous theme')
    run('xdotool', 'keydown', 'Alt_L')
    time.sleep(.8)
    screenshot('latte-switcher')
    run('xdotool', 'keyup', 'Alt_L')

    theme.write_text('foreground_color: [')
    wait(lambda: 'Theme/config reload:' in (work / 'app.log').read_text(), 'invalid edit was not reported')
    assert background_is((239, 241, 245)), 'invalid edit replaced the last good theme'
    theme.write_text((root / 'themes/catppuccin-mocha.yml').read_text())
    wait(lambda: background_is((30, 30, 46)), 'theme did not recover after a valid edit')
    screenshot('mocha')

    config.write_text(base + 'theme: badwolf\n')
    key('ctrl+shift+r')
    wait(lambda: background_is((20, 20, 19)), 'config reload did not select a preset')
    screenshot('badwolf')
    print('PASS: rendered ANSI colors, OSC overrides/reset, dark/light themes, atomic live reload,')
    print('      both tabs, Alt-held switcher, invalid edit recovery, and config reload')
    print('Artifacts:', work)
finally:
    app.terminate()
    try:
        app.wait(timeout=3)
    except sp.TimeoutExpired:
        app.kill()
        app.wait()
    log.close()
