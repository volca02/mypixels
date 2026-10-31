import os, pathlib, sys, tty
tty.setraw(0)
path = pathlib.Path(sys.argv[1]) / f"input-{os.getpid()}"
path.touch()
os.write(1, b'\x1b[?2004hA alpha beta\r\nsecond line\r\n')
while True:
    data = os.read(0, 65536)
    if not data:
        break
    with path.open('ab') as out:
        out.write(data)
    if data == b'\x05':
        os.write(1, ''.join(f'history {i:03}\r\n' for i in range(120)).encode())
    if data == b'm':
        # drag (1002) + SGR (1006): the smoke test then checks that the
        # terminal receives mouse reports instead of doing a selection
        os.write(1, b'\r\n\x1b[?1002h\x1b[?1006hMOUSE')
