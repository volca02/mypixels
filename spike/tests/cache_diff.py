"""Differential check of the damage-tracking frame cache.

Runs an identical scripted session (scrollback paging, zoom while scrolled,
less on the alt screen, vim editing) against two binaries -- a pre-cache
oracle and the current build -- and pixel-compares one screenshot per step.
Cell content is deterministic for both runs; the only expected difference is
cursor blink phase, worth about one cell, hence the 0.15% pixel tolerance.

Usage:
    OLD_BIN=... NEW_BIN=... xvfb-run -a python3 spike/tests/cache_diff.py
"""
import os
import re
import subprocess as sp
import sys
import tempfile
import time
from pathlib import Path

from PIL import Image

root = Path(__file__).resolve().parents[1]
OLD = os.environ.get('OLD_BIN', str(root / 'target/debug/spike'))
NEW = os.environ.get('NEW_BIN', str(root / 'target/release/spike'))
# Cursor blink is a free-running phase between runs (and the strip's clock
# sits just outside the crop); a cell plus antialiasing is ~0.06%. Half a
# percent still catches any real regression: the pre-cache-vs-cache bugs
# this test was written for measured 3-11%.
TOLERANCE = 0.005

CONFIG = """term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1000
default_height: 600
default_maximize: false
term_opacity: 0
hide_single_tab: false
# The default background tint (a deliberate post-oracle change) would make
# every pixel differ from the oracle's black for reasons this test is not
# about. Pin it black: parity here means TERMINAL content parity.
bg_tint_color: "#000000"
"""


def run_session(binary: str, tag: str) -> tuple[Path, dict]:
    """Drive one full session against `binary`; return (workdir, screenshots)."""
    work = Path(tempfile.mkdtemp(prefix=f'mypixels-cache-{tag}-'))
    (work / '.mypixels').write_text(CONFIG)
    keys = work / 'keys'
    os.mkfifo(keys)
    env = dict(os.environ, HOME=str(work), SPIKE_CONFIG=str(work / '.mypixels'),
               SPIKE_KEYS=str(keys), RUST_LOG='info', HISTFILE=str(work / '.history'))
    env.pop('WAYLAND_DISPLAY', None)
    log = (work / 'app.log').open('w')
    app = sp.Popen([binary, 'sh'], env=env, stdout=log, stderr=log)

    def keys_write(lines):
        with keys.open('w') as w:
            w.write(''.join(l + '\n' for l in lines))

    def type_(s: str):
        # The FIFO reader trims empty lines, so a literal space must travel
        # as its spec name; single characters map 1:1.
        keys_write(['space' if c == ' ' else c for c in s])

    def press(*specs: str, settle: float = 0.0):
        keys_write(list(specs))
        if settle:
            time.sleep(settle)

    def run_cmd(text: str):
        # Submit with two returns: a newline swallowed while the prompt was
        # still being written (slow debug shells) glued commands together.
        type_(text)
        press('return', settle=0.4)
        press('return', settle=0.4)
        if settle:
            time.sleep(settle)

    def grab(w: int, h: int) -> Image.Image:
        path = work / 'grab.png'
        sp.run(['import', '-window', 'root', str(path)], env=env, check=True, timeout=5)
        img = Image.open(path).convert('RGB')
        return img.crop((0, strip0, w, h))

    strip0 = 0  # set once geometry is parsed

    def quiet(w: int, h: int, secs: float = 15.0) -> None:
        """Block until two consecutive grabs (0.4s apart) are identical: the
        screen has settled. Sleeping a fixed time instead raced slow shells:
        keystrokes landing before a prompt is ready glue commands together."""
        prev = grab(w, h)
        t0 = time.time()
        while time.time() - t0 < secs:
            time.sleep(0.4)
            now = grab(w, h)
            if now.tobytes() == prev.tobytes():
                return
            prev = now

    def shot(name: str, w: int, h: int) -> tuple[str, Image.Image]:
        quiet(w, h)
        img = grab(w, h)
        path = work / f'{name}.png'
        img.save(path)
        return name, img

    def parse_geom() -> tuple[int, int, int]:
        info = (work / 'app.log').read_text()
        w, h = map(int, re.search(r'surf=(\d+)x(\d+)', info).groups())
        strip = round(float(re.search(r'strip = ([0-9.]+)px', info).group(1)))
        return w, h, strip

    shots: dict[str, Image.Image] = {}
    try:
        for _ in range(80):
            keys_write(['spy'])
            time.sleep(0.1)
            if 'SPY active=' in (work / 'app.log').read_text():
                break
        assert app.poll() is None, 'app died at boot:\n' + (work / 'app.log').read_text()[-2000:]
        w, h, strip0 = parse_geom()
        quiet(w, h)

        steps = [
            ('seq',        lambda: run_cmd('seq 1 400'),                              2.0),
            # Plain pgup belongs to the shell (bash: previous-history -- it
            # *types* into the prompt); spike scrolls its scrollback on
            # shift+pgup, matching the SHIFT+Page_Up default binding.
            ('scrolled',   lambda: press(*(['shift+pgup'] * 3), settle=0.5),           0.8),
            ('scrolled2',  lambda: press(*(['shift+pgup'] * 2)),                       0.8),
            ('live',       lambda: press(*(['shift+pgdn'] * 5)),                       0.8),
            ('zoom',       lambda: press(*(['alt+shift+plus'] * 2)),                   0.8),
            ('unzoom',     lambda: press(*(['alt+shift+minus'] * 2)),                  0.8),
            ('less',       lambda: run_cmd('less /etc/passwd'),                        1.5),
            ('less-end',   lambda: press('G', 'G'),                                    0.8),
            ('less-top',   lambda: press('g', 'g', 'g'),                               0.8),
            ('main',       lambda: press('q'),                                         1.0),
            ('vim-insert', lambda: (run_cmd('vim -u NONE'),
                                    press('i'), type_('h\u00e9llo \u754c world'),
                                    press('escape')),                                  0.8),
            ('vim-num',    lambda: (type_(':set nu'), press('return', settle=0.5)),    0.8),
            ('vim-bottom', lambda: press('G', 'G'),                                    0.8),
            ('vim-top',    lambda: press('g', 'g', 'g', 'g'),                          0.8),
            ('back-main',  lambda: run_cmd(':q!'),                                 1.0),
            ('end',        lambda: run_cmd('echo CACHE-DIFF-DONE'),                1.2),
        ]
        for name, action, settle in steps:
            action()
            time.sleep(settle)
            n, img = shot(name, w, h)
            shots[n] = img
        return work, shots
    finally:
        app.terminate()
        try:
            app.wait(3)
        except sp.TimeoutExpired:
            app.kill()


def compare(name: str, a: Image.Image, b: Image.Image) -> float:
    """Fraction of terminal pixels differing at all (strict per-channel)."""
    pa, pb = list(a.get_flattened_data()), list(b.get_flattened_data())
    assert a.size == b.size, (name, a.size, b.size)
    return sum(1 for x, y in zip(pa, pb) if x != y) / len(pa)


def main():
    print(f'oracle: {OLD}\ncurrent: {NEW}')
    work_a, shots_a = run_session(OLD, 'old')
    print(f'oracle done -> {work_a}')
    work_b, shots_b = run_session(NEW, 'new')
    print(f'current done -> {work_b}')
    failed = False
    for name in shots_a:
        pct = compare(name, shots_a[name], shots_b[name])
        status = 'ok ' if pct <= TOLERANCE else 'FAIL'
        print(f'  {status} {name:12} diff={pct * 100:.3f}% (tol {TOLERANCE * 100:.2f}%)')
        failed |= pct > TOLERANCE
    if failed:
        print(f'screenshots: {work_a} vs {work_b}')
    print('CACHE-DIFF: ' + ('FAIL' if failed else 'PASS'))
    sys.exit(1 if failed else 0)


main()
