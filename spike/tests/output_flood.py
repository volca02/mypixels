"""Changing path output for terminal throughput/frame-cadence checks.

Usage: python3 output_flood.py SECONDS STATS_JSON
Run inside a disposable terminal; this intentionally produces unpaced output.
"""
import json, os, pathlib, sys, time
duration=float(sys.argv[1])
stat=pathlib.Path(sys.argv[2])
start=time.monotonic()
count=0
while time.monotonic()-start < duration:
    batch=''.join(f'/home/volca/Projects/example/node_modules/package-{count+i:08d}/lib/component/implementation-{count+i:08d}.js\r\n' for i in range(500)).encode()
    offset=0
    while offset<len(batch):
        offset+=os.write(1,batch[offset:])
    count+=500
stat.write_text(json.dumps(dict(lines=count,seconds=time.monotonic()-start)))
time.sleep(3)
