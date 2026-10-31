import os, pathlib, re, subprocess as sp, tempfile, time

root = pathlib.Path(__file__).resolve().parents[2]
assert os.environ.get('DISPLAY'), 'Run with xvfb-run -a python3 spike/tests/ui_smoke.py'
work = pathlib.Path(tempfile.mkdtemp(prefix='mypixels-ui-'))
(work / '.mypixels').write_text('term_font: Cascadia Code 9\ntablist_font: Terminus (TTF) 12\ndefault_width: 800\ndefault_height: 480\ndefault_maximize: false\nterm_opacity: 0\nhide_single_tab: false\n')
helpers = work / 'helpers'
helpers.mkdir()
delayed = helpers / 'xsel'
delayed.write_text('#!/usr/bin/python3\nimport os,sys,time\nif \"-o\" in sys.argv: time.sleep(.3)\nos.execv(\"/usr/bin/xsel\", [\"xsel\"]+sys.argv[1:])\n')
delayed.chmod(0o755)
env = dict(os.environ, HOME=str(work), RUST_LOG='info', PATH=str(helpers)+os.pathsep+os.environ['PATH'])
env.pop('WAYLAND_DISPLAY', None)
env.pop('SPIKE_KEYS', None)
log = (work / 'app.log').open('w')
app = sp.Popen([str(root / 'spike/target/debug/spike'), '/usr/bin/python3', str(root / 'spike/tests/pty_fixture.py'), str(work)], env=env, stdout=log, stderr=log)

def run(*args, **kw):
    if args[0] == 'xsel': args = ('/usr/bin/xsel', *args[1:])
    return sp.run(args, env=env, check=True, stdout=sp.PIPE, stderr=sp.PIPE, timeout=4, **kw).stdout

def wait(check, message):
    until=time.monotonic()+5
    while time.monotonic()<until:
        value=check()
        if value:
            return value
        assert app.poll() is None, (work/'app.log').read_text()
        time.sleep(.025)
    raise AssertionError(message+'\n'+(work/'app.log').read_text())

try:
    def window():
        result=sp.run(['xdotool','search','--onlyvisible','--pid',str(app.pid)], env=env, stdout=sp.PIPE)
        return result.stdout.decode().splitlines()
    wid=wait(window,'window did not appear')[-1]
    wait(lambda:list(work.glob('input-*')), 'PTY did not start')
    capture=next(work.glob('input-*'))
    metrics=re.search(r'cell = ([0-9.]+) x ([0-9.]+),.*strip = ([0-9.]+)px', (work/'app.log').read_text())
    cw,ch,strip=map(float,metrics.groups())
    run('xdotool','windowfocus','--sync',wid)

    def mouse(x,y): run('xdotool','mousemove','--window',wid,str(round(x)),str(round(y)))
    def key(k): run('xdotool','key',k)
    def primary(): return run('xsel','-p','-o').decode()
    def select_first_character():
        mouse(1,strip+ch/2)
        run('xdotool','mousedown','1')
        mouse(cw-1,strip+ch/2)
        run('xdotool','mouseup','1')

    select_first_character()
    wait(lambda:primary()=='A','single-character primary selection failed')
    key('ctrl+shift+c')
    wait(lambda:run('xsel','-b','-o')==b'A','Ctrl+Shift+C failed')
    text='č 🦀\r\nnext\rline\n'
    run('xsel','-b','-i',input=text.encode())
    key('ctrl+shift+v')
    expected='\x1b[200~č 🦀\nnext\nline\n\x1b[201~'.encode()
    wait(lambda:capture.read_bytes()==expected,'bracketed clipboard paste failed')
    assert primary()=='A', 'clipboard paste changed primary selection'
    mouse(100,strip+ch*3)
    run('xdotool','click','2')
    expected+=b'\x1b[200~A\x1b[201~'
    wait(lambda:capture.read_bytes()==expected,'middle-click primary paste failed')
    time.sleep(.45)
    mouse(cw*4,strip+ch/2)
    run('xdotool','click','--repeat','2','--delay','80','1')
    wait(lambda:primary()=='alpha','double-click word selection failed')
    time.sleep(.45)
    mouse(cw*4,strip+ch*1.5)
    run('xdotool','click','--repeat','3','--delay','80','1')
    wait(lambda:primary()=='second line\n','triple-click line newline failed')
    key('ctrl+e')
    time.sleep(.25)
    key('shift+Prior')
    time.sleep(.2)
    mouse(cw*4,strip+ch/2)
    run('xdotool','click','--repeat','3','--delay','80','1')
    wait(lambda:primary().startswith('history '),'scrollback selection failed')
    run('import','-window',wid,str(work/'ui.png'))
    # Delay the helper read, then switch tabs before the result arrives.
    payload = 'delayed destination'
    run('xsel','-b','-i',input=payload.encode())
    key('ctrl+shift+v')
    key('ctrl+shift+t')
    wait(lambda:len(list(work.glob('input-*')))==2, 'new tab shortcut failed')
    wait(lambda:payload.encode() in capture.read_bytes(), 'paste lost its requesting tab')
    other = next(p for p in work.glob('input-*') if p != capture)
    assert other.read_bytes()==b'', 'paste was redirected into the new tab'
    # The useful Alt-held visual switcher stays. Only standalone overview was
    # retired. Keep a visual artifact, and exercise selecting on Alt release.
    run('xdotool','keydown','Alt_L')
    time.sleep(.8)
    run('import','-window',wid,str(work/'switcher.png'))
    run('xdotool','key','Left')
    run('xdotool','keyup','Alt_L')
    key('z')
    wait(lambda:capture.read_bytes().endswith(b'z'), 'Alt-held switcher did not select the first tab')
    assert other.read_bytes()==b'', 'switcher typed into the wrong tab'
    key('alt+o')
    wait(lambda:capture.read_bytes().endswith(b'\x1bo'), 'retired standalone overview still swallowed Alt+O')
    print('PASS: keyboard copy/paste, Unicode, bracketed paste, primary, selection, scrollback, delayed paste destination, and Alt-held visual switcher')
    print('Artifacts:',work)
finally:
    app.terminate()
    try: app.wait(timeout=3)
    except sp.TimeoutExpired: app.kill(); app.wait()
    log.close()
