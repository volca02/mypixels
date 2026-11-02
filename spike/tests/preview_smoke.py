"""Compare the switcher thumbnail with a downscaled live terminal frame.

Run: xvfb-run -a python3 tests/preview_smoke.py
Requires ImageMagick import and Pillow; uses an isolated HOME and X display.
"""
import os
import re
from pathlib import Path
import subprocess as sp
import tempfile
import time

from PIL import Image, ImageChops, ImageStat

root = Path(__file__).resolve().parents[1]
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'
work = Path(tempfile.mkdtemp(prefix='mypixels-preview-'))
(work / '.mypixels').write_text('''term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1000
default_height: 600
default_maximize: false
term_opacity: 0
hide_single_tab: false
background_color: '#122e2e'
''')
fixture = work / 'fixture.py'
fixture.write_text(r'''
import os, time
rows, cols = os.get_terminal_size().lines, os.get_terminal_size().columns
bg = '\x1b[48;2;18;46;46m'
yellow = '\x1b[38;2;255;214;64m'
lines = [yellow + '█' * cols]
for row in range(1, rows - 1):
    lines.append(yellow + '█' + ' ' * (cols - 2) + '█')
lines.append(yellow + '█' * cols)
os.write(1, ('\x1b[?25l\x1b[2J\x1b[H' + bg + '\r\n'.join(lines)
    + '\x1b[4;4H\x1b[38;2;255;60;80mRed text  \x1b[38;2;60;240;100mGreen text'
    + '\x1b[6;4H\x1b[1;38;2;80;160;255mBold blue  界 e\u0301  aligned\x1b[22m').encode())
while True:
    time.sleep(1)
''')
keys = work / 'keys'
os.mkfifo(keys)
env = dict(os.environ, HOME=str(work), SPIKE_CONFIG=str(work / '.mypixels'),
           SPIKE_KEYS=str(keys), RUST_LOG='info')
env.pop('WAYLAND_DISPLAY', None)
log = (work / 'app.log').open('w')
app = sp.Popen([str(root / 'target/debug/spike'), 'python3', str(fixture)],
               env=env, stdout=log, stderr=log)


def screenshot(name):
    path = work / (name + '.png')
    sp.run(['import', '-window', 'root', str(path)], env=env, check=True, timeout=5)
    return Image.open(path).convert('RGB')


def yellow_box(image):
    mask = image.point(lambda _: 0).convert('L')
    mask.putdata([255 if r > 180 and g > 140 and b < 100 else 0
                  for r, g, b in list(image.get_flattened_data())])
    return mask.getbbox()


try:
    time.sleep(2)
    assert app.poll() is None, (work / 'app.log').read_text()
    with keys.open('w') as writer:
        writer.write('spy\n')
    time.sleep(.2)
    info = (work / 'app.log').read_text()
    w, h = map(int, re.search(r'surf=(\d+)x(\d+)', info).groups())
    strip = round(float(re.search(r'strip = ([0-9.]+)px', info).group(1)))
    live = screenshot('live')
    source_box = (0, strip, w, h)
    with keys.open('w') as writer:
        writer.write('switcher\n')
    time.sleep(1)
    overlay = screenshot('switcher')
    # The dimmed terminal's yellow is below the mask threshold; only the
    # opaque thumbnail frame remains bright yellow.
    thumb_box = yellow_box(overlay.crop((0, strip, w, h)))
    if thumb_box:
        thumb_box = (thumb_box[0], thumb_box[1] + strip, thumb_box[2], thumb_box[3] + strip)
    assert thumb_box, 'preview frame missing'
    width, height = thumb_box[2] - thumb_box[0], thumb_box[3] - thumb_box[1]
    assert abs(width - min(300, w / 4 - 20)) <= 3, thumb_box
    source = live.crop(source_box)
    assert abs(width / height - source.width / source.height) < .04, (source_box, thumb_box)
    expected = source.resize((width, height), Image.Resampling.BOX)
    actual = overlay.crop(thumb_box)
    expected.save(work / 'expected.png')
    actual.save(work / 'actual.png')
    error = ImageStat.Stat(ImageChops.difference(expected, actual)).mean
    assert max(error) < 12, f'preview differs from live frame: mean RGB error {error}'
    # Both vertical sides must reach the image edges, detecting compressed
    # text placement even when the overall thumbnail rectangle is correct.
    for x in (0, width - 1):
        edge = actual.crop((x, 0, x + 1, height))
        assert sum(r > 180 and g > 140 and b < 100 for r, g, b in edge.get_flattened_data()) > height * .8
    print('PASS: complete frame, aspect ratio, glyph placement, and colors', error)
    print('Artifacts:', work)
finally:
    app.terminate()
    try:
        app.wait(timeout=3)
    except sp.TimeoutExpired:
        app.kill()
        app.wait()
    log.close()
