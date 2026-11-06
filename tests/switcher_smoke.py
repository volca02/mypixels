"""Real switcher interaction and visual artifacts with eight live terminals.

Run with xvfb-run -a python3 tests/switcher_smoke.py. Uses an isolated HOME.
"""
import os
from pathlib import Path
import re
import subprocess as sp
import tempfile
import time

from PIL import Image

root = Path(__file__).resolve().parents[1]
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'
work = Path(tempfile.mkdtemp(prefix='mypixels-switcher-'))
config = work / 'config.yml'
config.write_text('''wal_palette: ''
term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1500
default_height: 900
default_maximize: false
hide_single_tab: false
term_opacity: 0
bg_tint_color: '#0b1216'
background_color: '#0b1216'
foreground_color: '#dce4e7'
accent_color: '#00dbe6'
tab_overview_opacity: 0.65
''')
fixture = work / 'fixture.py'
fixture.write_text(r'''
import os, sys, tty
from pathlib import Path
work = Path(sys.argv[1])
index = len(list(work.glob('input-*')))
path = work / f'input-{index}'
path.touch()
tty.setraw(0)
names = ['build', 'editor', 'tests', 'server', 'logs', 'git', 'python', 'ssh']
commands = ['pnpm run build', 'nvim src/main.rs', 'pnpm test', 'pnpm dev', 'tail -f app.log', 'git status', 'python', 'ssh dev']
lines = [
    '> vite build\r\n\r\n\x1b[36mvite v5.4.2\x1b[0m building for production...\r\n\x1b[32m✓\x1b[0m 1,243 modules transformed.\r\n\x1b[32m✓\x1b[0m rendering chunks...\r\n\x1b[32m✓\x1b[0m built in 8.4s',
    '142  \x1b[31mexport function\x1b[0m renderTerm(\r\n143    ctx: RenderContext,\r\n144    grid: CellGrid,\r\n145  ) {\r\n146    \x1b[35mfor\x1b[0m (let y = 0; y < grid.h; y++) {\r\n147      const row = grid.rows[y];\r\n148      drawRow(ctx, row);\r\n149    }\r\n150  }',
    '\x1b[36mvitest v1.6.0\x1b[0m\r\n\x1b[32m✓\x1b[0m renderer/terminal.ts (18)\r\n\x1b[32m✓\x1b[0m core/grid.test.ts (9)\r\n\x1b[32m✓\x1b[0m ansi/parser.test.ts (6)\r\n\r\nTest Files  \x1b[32m4 passed\x1b[0m (4)\r\nDuration    1.32s',
    '\x1b[36mvite v5.4.2\x1b[0m ready in 312 ms\r\n\r\n  Local:   \x1b[36mhttp://localhost:5173/\x1b[0m\r\n  Network: \x1b[36mhttp://192.168.1.42:5173/\x1b[0m\r\n\r\n[vite] hmr update /src/ui/switcher.ts',
    '14:36:51  \x1b[36mINFO\x1b[0m session created\r\n14:36:53  \x1b[36mINFO\x1b[0m tab added #7 (python)\r\n14:36:59  \x1b[33mWARN\x1b[0m high render latency: 42ms\r\n14:37:01  \x1b[36mINFO\x1b[0m switcher opened (8 tabs)\r\n14:37:03  \x1b[36mINFO\x1b[0m preview updated (4.3ms)',
    "On branch main\r\nYour branch is up to date with 'origin/main'.\r\n\r\nChanges not staged for commit:\r\n  \x1b[31mmodified:  src/ui/switcher.ts\x1b[0m\r\n  \x1b[31mmodified:  src/renderer/grid.ts\x1b[0m\r\n\r\nno changes added to commit",
    'Python 3.12.2 on linux\r\nType "help", "copyright", "credits" or "license".\r\n>>> from mypixels import terminal\r\n>>> terminal.cells\r\n(240, 60)\r\n>>>',
    'top - 14:37:12 up 12 days, 4:21, 2 users\r\nTasks: 143 total, 1 running, 142 sleeping\r\n%Cpu(s): 4.8 us, 1.2 sy, 0.0 ni, 93.5 id\r\nMiB Mem: 31952.0 total, 8211.4 free\r\n\r\n  PID USER    %CPU %MEM COMMAND\r\n 5284 ubuntu  12.3  1.4 node\r\n 6149 volca    4.7  0.9 mypixels',
]
name = names[index % 8]
command = commands[index % 8]
os.write(1, (f'\x1b]2;{name}\x07\x1b[?25l\x1b]133;A\x07> \x1b]133;B\x07{command}\x1b]133;C\x07\r\n' + lines[index % 8] + '\r\n').encode())
if index in (0, 2, 5):
    os.write(1, b'\x1b]133;D;0\x07\x1b]133;A\x07> \x1b]133;B\x07')
while True:
    data = os.read(0, 65536)
    if not data: break
    with path.open('ab') as out: out.write(data)
''')
env = dict(os.environ, HOME=str(work), XDG_CONFIG_HOME=str(work / '.config'),
           MYPIXELS_CONFIG=str(config), RUST_LOG='info')
env.pop('WAYLAND_DISPLAY', None)
env.pop('MYPIXELS_KEYS', None)
binary = os.environ.get('MYPIXELS_BIN', str(root / 'target/debug/mypixels'))
log_path = work / 'app.log'
log = log_path.open('w')
app = sp.Popen([binary, '-s', 'python3', str(fixture), str(work)], env=env, stdout=log, stderr=log)


def run(*args):
    return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=5).stdout


def wait(check, message):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        value = check()
        if value: return value
        assert app.poll() is None, log_path.read_text()
        time.sleep(.08)
    raise AssertionError(message + '\n' + log_path.read_text())


def shot(name):
    time.sleep(.45)
    path = work / (name + '.png')
    run('import', '-window', wid, str(path))
    return Image.open(path).convert('RGB')


def key(value):
    run('xdotool', 'key', value)


def opened():
    run('xdotool', 'keydown', 'Alt_L')
    time.sleep(.9)


try:
    def windows():
        return sp.run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)],
                      env=env, stdout=sp.PIPE).stdout.decode().splitlines()
    wid = wait(windows, 'window missing')[-1]
    wait(lambda: (work / 'input-0').exists(), 'PTY missing')
    run('xdotool', 'windowfocus', '--sync', wid)
    time.sleep(.3)
    for count in range(2, 9):
        key('ctrl+shift+t')
        wait(lambda: (work / f'input-{count-1}').exists(), 'new tab missing')
    key('alt+1')
    time.sleep(.3)
    opened()
    run('xdotool', 'mousemove', '--window', wid, '10', '80')
    def switcher_visible():
        image = shot('eight-tabs')
        # The first Area pass only measures the surface. Wait for a painted
        # selection rather than assuming preview creation finishes in .9s.
        cyan = any((lambda p: p[0] < 80 and p[1] > 180 and p[2] > 180)(image.getpixel((x, y)))
                   for y in range(100, image.height - 80, 2) for x in range(0, image.width, 2))
        return image if cyan else None
    image = wait(switcher_visible, 'selected card outline missing')
    key('Tab')
    key('shift+Tab')
    key('Right')
    key('Return')
    run('xdotool', 'keyup', 'Alt_L')
    key('x')
    wait(lambda: (work / 'input-1').read_bytes() == b'x', 'Tab/Shift-Tab/Enter selected the wrong tab')
    assert (work / 'input-0').read_bytes() == b'', 'switcher keys leaked into PTY'
    opened()
    key('Right')
    key('Escape')
    run('xdotool', 'keyup', 'Alt_L')
    key('y')
    wait(lambda: (work / 'input-1').read_bytes() == b'xy', 'Escape did not restore the original tab')
    # The selected card's outline locates its hit target without depending on text metrics.
    opened()
    selected = shot('second-selected')
    # Find the long cyan top edge of the selected card. Preview text does
    # not contain a continuous cyan run this long.
    edge = None
    for y in range(100, selected.height - 80):
        start = None
        for x in range(selected.width):
            r, g, b = selected.getpixel((x, y))
            if r < 40 and g > 190 and b > 190:
                if start is None: start = x
            else:
                if start is not None and x - start > 170:
                    edge = (start, x, y)
                    break
                start = None
        if edge: break
    assert edge, 'selected card top edge missing'
    start, end, top = edge
    card_width = end - start + 14
    click_x = (start + end) / 2 - card_width - 14
    click_y = top + 65
    run('xdotool', 'mousemove', '--window', wid, str(round(click_x)), str(round(click_y)), 'click', '1')
    run('xdotool', 'keyup', 'Alt_L')
    key('z')
    wait(lambda: (work / 'input-0').read_bytes() == b'z', 'clicking a card did not activate it')
    # Opening over a focused history search must give navigation to the switcher.
    key('alt+shift+h')
    time.sleep(.4)
    run('xdotool', 'mousemove', '--window', wid, str(image.width - 64), '55', 'click', '1')
    time.sleep(.2)
    run('xdotool', 'type', 'search text')
    run('xdotool', 'windowsize', wid, '560', '480')
    opened()
    key('End')
    # Cached previews from the old size must not force the modal off-screen.
    panel_color = image.getpixel((image.width // 2, 180))
    def resized():
        small = shot('small-last-tab')
        return small.getpixel((small.width // 2, 35)) == panel_color
    wait(resized, 'switcher did not adapt to the smaller window')
    run('xdotool', 'keyup', 'Alt_L')
    key('q')
    wait(lambda: (work / 'input-7').read_bytes() == b'q', 'small-window navigation failed')
    print('PASS: eight live previews, Tab/Shift-Tab/Enter, Escape cancellation, card clicks, small-window navigation; artifacts:', work, flush=True)
finally:
    app.terminate()
    try: app.wait(timeout=3)
    except sp.TimeoutExpired:
        app.kill()
        app.wait()
    log.close()
