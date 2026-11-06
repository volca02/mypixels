"""Real shell history capture and terminal context menus beside the sidebar.

Run: xvfb-run -a python3 tests/history_smoke.py
Requires xdotool, xsel, ImageMagick import, and Pillow. Each shell gets an
isolated HOME, prompt, and config; the desktop clipboard is never used.
"""
import os
from pathlib import Path
import re
import shutil
import subprocess as sp
import tempfile
import time

from PIL import Image, ImageChops

root = Path(__file__).resolve().parents[1]
binary = Path(os.environ.get('MYPIXELS_BIN', root / 'target/debug/mypixels')).resolve()
assert os.environ.get('DISPLAY'), 'Run under xvfb-run'


def check_shell(shell):
    work = Path(tempfile.mkdtemp(prefix=f'mypixels-history-{shell}-'))
    keys = work / 'keys'
    os.mkfifo(keys)
    config = work / 'config.yml'
    config.write_text('''wal_palette: ''
term_font: Cascadia Code 12
tablist_font: Terminus (TTF) 12
default_width: 1100
default_height: 700
default_maximize: false
term_opacity: 0
hide_single_tab: false
''')
    integration = root / f'scripts/shell_integration.{shell}'
    if shell == 'zsh':
        (work / '.zshrc').write_text(
            "PS1=$'界 project\\n❯ '\n"
            f"source {integration}\nsource {integration}\n")
        args = ['zsh', '-d', '-i']
    elif shell == 'bash':
        rc = work / 'bashrc'
        rc.write_text("PS1='界 project\\n❯ '\n" + f"source {integration}\nsource {integration}\n")
        args = ['bash', '--noprofile', '--rcfile', str(rc), '-i']
    else:
        init = work / 'init.fish'
        init.write_text("function fish_prompt; printf '界 project\\n❯ '; end\n"
                        f"source {integration}\nsource {integration}\n")
        args = ['fish', '--no-config', '--interactive', '--init-command', f'source {init}']
    env = dict(os.environ, HOME=str(work), ZDOTDIR=str(work), XDG_CONFIG_HOME=str(work / '.config'),
               MYPIXELS_CONFIG=str(config), MYPIXELS_KEYS=str(keys),
               RUST_LOG='info,mypixels::tab=debug')
    env.pop('WAYLAND_DISPLAY', None)
    log_path = work / 'app.log'
    log = log_path.open('w')
    app = sp.Popen([str(binary), '-s', *args], env=env, stdout=log, stderr=log)

    def run(*args):
        return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=5).stdout

    def wait(check, message):
        until = time.monotonic() + 8
        while time.monotonic() < until:
            value = check()
            if value:
                return value
            assert app.poll() is None, log_path.read_text()
            time.sleep(.05)
        raise AssertionError(f'{shell}: {message}\n{log_path.read_text()}')

    def send(text):
        with keys.open('w') as out:
            out.write(text + '\n')

    def shot(name):
        time.sleep(.3)
        path = work / (name + '.png')
        run('import', '-window', wid, str(path))
        return Image.open(path).convert('RGB')

    try:
        def windows():
            result = sp.run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)],
                            env=env, stdout=sp.PIPE)
            return result.stdout.decode().splitlines()
        wid = wait(windows, 'window missing')[-1]
        run('xdotool', 'windowfocus', '--sync', wid)
        wait(lambda: 'code 1 ' in log_path.read_text(), 'prompt-end marker missing')
        prompt_count = log_path.read_text().count('code 1 ')
        count = work / 'executions'
        command = f"printf 'CAPTURED-HISTORY\\n'; printf x >> {count}"
        send('line:' + command)
        wait(lambda: count.exists() and count.read_text() == 'x', 'command did not run')
        wait(lambda: 'code 2 ' in log_path.read_text(), 'execution marker missing')
        wait(lambda: log_path.read_text().count('code 1 ') > prompt_count, 'next prompt missing')
        terminal = shot('terminal')
        send('history')
        probe = (terminal.width - 8, terminal.height // 2)
        def sidebar_visible():
            current = shot('history')
            return current if current.getpixel(probe) != terminal.getpixel(probe) else None
        before = wait(sidebar_visible, 'history sidebar did not appear')
        # Move from the sidebar to the terminal and click without waiting
        # for a UI pass, exercising stale egui pointer state as well.
        run('xdotool', 'mousemove', '--window', wid, '900', '80')
        time.sleep(.1)
        # A root layout claimed this right click before the fix.
        run('xdotool', 'mousemove', '--window', wid, '150', '180', 'click', '3')
        menu = shot('context-menu')
        changed = ImageChops.difference(before, menu).crop((150, 180, 380, 500)).convert('L')
        assert sum(value > 15 for value in changed.get_flattened_data()) > 1000, 'terminal context menu did not open'
        run('xdotool', 'key', 'Escape')
        after = shot('menu-dismissed')
        assert ImageChops.difference(before, after).crop((760, 40, 1080, 120)).getbbox() is None, 'dismissing menu also closed history'
        run('xdotool', 'key', 'ctrl+shift+l')
        output = wait(lambda: run('xsel', '-b', '-o').decode() or None, 'output clipboard empty')
        assert 'CAPTURED-HISTORY' in output, output
        assert log_path.read_text().count('code 2 ') == 1, 'sourcing integration twice duplicated execution hooks'
        # Re-run uses the command captured from the grid at C. A multiline
        # prompt or a stale '(running)' row must not replace the command.
        strip = float(re.search(r'strip = ([0-9.]+)px', log_path.read_text()).group(1))
        rerun_y = round(strip + 18 + 32 + 16 + 34 + 18 + 38 + 18 + 170 + 42 + 2 * 37 + 17)
        run('xdotool', 'mousemove', '--window', wid, '900', str(rerun_y), 'click', '1')
        wait(lambda: count.read_text() == 'xx', 're-run did not execute the captured command')
        send('line:false')
        wait(lambda: 'code 3 exit Some(1)' in log_path.read_text(), 'failed exit status missing')
        shot('finished')
        print(f'PASS: {shell} command capture, completion, re-run, and context menu; artifacts: {work}', flush=True)
    finally:
        app.terminate()
        try:
            app.wait(timeout=3)
        except sp.TimeoutExpired:
            app.kill()
            app.wait()
        log.close()


for shell in ('zsh', 'bash', 'fish'):
    if shutil.which(shell):
        check_shell(shell)
    else:
        print(f'SKIP: {shell} is not installed')
