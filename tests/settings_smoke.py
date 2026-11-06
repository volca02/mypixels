"""Settings navigation, popups, pinned actions, Save, and Cancel.

Run under an isolated X server: xvfb-run -a python3 tests/settings_smoke.py
Requires xdotool, ImageMagick import, and Pillow. Uses an isolated HOME/config.
"""
import os
from pathlib import Path
import re
import subprocess as sp
import tempfile
import time

from PIL import Image, ImageChops, ImageStat

root = Path(__file__).resolve().parents[1]
binary = Path(os.environ.get('MYPIXELS_BIN', root / 'target/debug/mypixels')).resolve()
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'
work = Path(tempfile.mkdtemp(prefix='mypixels-settings-'))
config = work / 'config.yml'
config.write_text('''# Preserve this comment when saving.
wal_palette: ''
term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1280
default_height: 850
default_maximize: false
term_opacity: 0
hide_single_tab: false
background_color: '#0b1216'
foreground_color: '#dce4e7'
accent_color: '#00dbe6'
''')
env = dict(os.environ, HOME=str(work), XDG_CONFIG_HOME=str(work / '.config'),
           MYPIXELS_CONFIG=str(config), RUST_LOG='info')
env.pop('WAYLAND_DISPLAY', None)
env.pop('MYPIXELS_KEYS', None)
log_path = work / 'app.log'
log = log_path.open('w')
app = sp.Popen([str(binary), '-s', 'python3', str(root / 'tests/pty_fixture.py'), str(work)],
               env=env, stdout=log, stderr=log)


def run(*args):
    return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=5).stdout


def wait(check, message):
    until = time.monotonic() + 10
    while time.monotonic() < until:
        value = check()
        if value:
            return value
        assert app.poll() is None, log_path.read_text()
        time.sleep(.08)
    raise AssertionError(message + '\n' + log_path.read_text())


def shot(name):
    time.sleep(.4)
    path = work / (name + '.png')
    run('import', '-window', wid, str(path))
    return Image.open(path).convert('RGB')


def click(x, y):
    run('xdotool', 'mousemove', '--window', wid, str(round(x)), str(round(y)), 'click', '1')
    time.sleep(.15)


def page(offset):
    click(left + offset, strip + 105)


def edit_width(value):
    run('xdotool', 'mousemove', '--window', wid, str(round(left + 235)), str(round(strip + 244)),
        'click', '--repeat', '2', '--delay', '70', '1')
    run('xdotool', 'key', 'ctrl+a')
    run('xdotool', 'type', str(value))
    run('xdotool', 'key', 'Return')
    time.sleep(.2)


try:
    def windows():
        result = sp.run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)], env=env, stdout=sp.PIPE)
        return result.stdout.decode().splitlines()
    wid = wait(windows, 'window did not appear')[-1]
    wait(lambda: list(work.glob('input-*')), 'PTY missing')
    run('xdotool', 'windowfocus', '--sync', wid)
    strip = float(re.search(r'strip = ([0-9.]+)px', log_path.read_text()).group(1))
    terminal = shot('terminal')
    width, height = terminal.size
    left = width - 448
    run('xdotool', 'key', 'alt+shift+c')
    probe = (width - 8, height // 2)
    def appeared():
        image = shot('appearance')
        return image if image.getpixel(probe) != terminal.getpixel(probe) else None
    wait(appeared, 'settings did not appear')
    page(156)
    shot('window')
    edit_width(1234)
    shot('edited')
    click(left + 330, height - 35)
    wait(lambda: 'default_width: 1234' in config.read_text(), 'Save did not persist the edited width')
    assert config.read_text().startswith('# Preserve this comment'), 'Save lost the configuration comment'
    saved = config.read_text()
    run('xdotool', 'key', 'alt+shift+c')
    time.sleep(.3)
    edit_width(9999)
    click(left + 118, height - 35)
    wait(lambda: 'configuration and colors reloaded' in log_path.read_text(), 'Cancel did not reload saved settings')
    assert config.read_text() == saved, 'Cancel modified the saved configuration'
    run('xdotool', 'key', 'alt+shift+c')
    page(224)
    shot('tabs')
    run('xdotool', 'mousemove', '--window', wid, str(width - 80), '620', 'click', '--repeat', '7', '--delay', '70', '5')
    shot('tabs-scrolled')
    page(296)
    shot('terminal-settings')
    page(67)
    click(left + 270, strip + 208)
    shot('font-picker')
    # Clicking outside the picker leaves settings open.
    click(left + 75, strip + 34)
    click(left + 215, strip + 389)
    color_popup = shot('color-picker')
    assert color_popup.getpixel((left - 30, height - 100)) != terminal.getpixel((left - 30, height - 100)), 'color picker missing'
    click(left + 75, strip + 34)
    settings = shot('settings-before-switch')
    run('xdotool', 'key', 'alt+shift+h')
    history = shot('history-after-settings')
    assert history.getpixel(probe) == settings.getpixel(probe), 'sidebar background colors differ'
    former_settings = (width - 420, height // 2)
    assert history.getpixel(former_settings) == terminal.getpixel(former_settings), 'settings remained behind history'
    run('xdotool', 'key', 'alt+shift+c')
    reopened = shot('settings-after-history')
    difference = ImageStat.Stat(ImageChops.difference(settings.crop((left, strip, width, height)), reopened.crop((left, strip, width, height)))).mean
    assert max(difference) < 2, f'history remained over settings: {difference}'
    run('xdotool', 'windowsize', wid, '900', '540')
    shot('small')
    assert all(not p.read_bytes() for p in work.glob('input-*')), 'settings input leaked into the terminal'
    print('PASS: settings pages, pinned actions, Save, Cancel, popups, exclusive sidebars, matching backgrounds; artifacts:', work, flush=True)
finally:
    app.terminate()
    try:
        app.wait(timeout=3)
    except sp.TimeoutExpired:
        app.kill()
        app.wait()
    log.close()
