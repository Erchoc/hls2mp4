#!/usr/bin/env python3
"""End-to-end local fixtures. Python and FFmpeg are needed only by this test."""
from collections import Counter
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / 'target/release/hls2mp4'
FFMPEG = shutil.which('ffmpeg')
assert FFMPEG, 'Fixture generation requires FFmpeg'


def ff(*args):
    p = subprocess.run([FFMPEG, '-hide_banner', '-loglevel', 'error', '-y', *map(str, args)],
                       capture_output=True)
    if p.returncode:
        raise AssertionError(p.stderr.decode())
    return p.stdout


with tempfile.TemporaryDirectory(prefix='hls2mp4-tests-') as d:
    root = Path(d)
    web = root / 'web'
    web.mkdir()
    ff('-f', 'lavfi', '-i', 'testsrc2=size=320x180:rate=24', '-f', 'lavfi', '-i',
       'sine=frequency=440:sample_rate=48000', '-t', '3', '-c:v', 'libx264', '-preset',
       'ultrafast', '-g', '24', '-c:a', 'aac', web / 'original.ts')
    key = web / 'key'
    key.write_bytes(bytes(range(16)))
    keyinfo = root / 'keyinfo'
    keyinfo.write_text(f'key\n{key}\n00000000000000000000000000000001\n')
    ff('-i', web / 'original.ts', '-c', 'copy', '-hls_time', '1', '-hls_key_info_file',
       keyinfo, '-hls_segment_filename', web / 'enc%d.ts', web / 'encrypted.m3u8')
    ff('-i', web / 'original.ts', '-c', 'copy', '-hls_time', '1', '-hls_segment_filename',
       web / 'clear%d.ts', web / 'clear.m3u8')
    ff('-i', web / 'original.ts', '-c', 'copy', '-bsf:a', 'aac_adtstoasc', '-hls_time', '1', '-hls_flags', 'single_file',
       '-hls_segment_type', 'fmp4', '-hls_segment_filename', web / 'range.mp4', web / 'range.m3u8')
    ff('-i', web / 'original.ts', '-an', '-c:v', 'copy', '-hls_time', '1',
       '-hls_segment_filename', web / 'v%d.ts', web / 'video.m3u8')
    ff('-i', web / 'original.ts', '-vn', '-c:a', 'copy', '-hls_time', '1',
       '-hls_segment_filename', web / 'a%d.ts', web / 'audio.m3u8')
    (web / 'master.m3u8').write_text('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=320x180,CODECS="avc1.42c00b,mp4a.40.2"\nclear.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,CODECS="avc1.42c00b,mp4a.40.2"\nencrypted.m3u8\n')
    (web / 'separate.m3u8').write_text('#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",DEFAULT=YES,NAME="English",URI="audio.m3u8"\n#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=320x180,AUDIO="audio"\nvideo.m3u8\n')
    (web / 'index.html').write_text('<iframe src="player.html"></iframe>')
    (web / 'player.html').write_text('<script>player={url:"/master.m3u8?token=test&amp;v=1"}</script>')
    (web / 'ambiguous.html').write_text('<a href="clear.m3u8">one</a><a href="encrypted.m3u8">two</a>')
    (web / 'live.m3u8').write_text((web / 'clear.m3u8').read_text().replace('#EXT-X-ENDLIST', ''))
    (web / 'drm.m3u8').write_text((web / 'encrypted.m3u8').read_text().replace('METHOD=AES-128', 'METHOD=SAMPLE-AES'))
    hits = Counter()
    fail_once = {'/enc0.ts'}

    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(web), **kwargs)

        def log_message(self, *args):
            pass

        def do_GET(self):
            path = self.path.split('?')[0]
            hits[path] += 1
            if path in fail_once:
                fail_once.remove(path)
                self.send_error(503)
                return
            if self.headers.get('Range'):
                content = (web / path.lstrip('/')).read_bytes()
                start, end = map(int, self.headers['Range'].removeprefix('bytes=').split('-'))
                part = content[start:end + 1]
                self.send_response(206)
                self.send_header('Content-Length', str(len(part)))
                self.send_header('Content-Range', f'bytes {start}-{end}/{len(content)}')
                self.end_headers()
                self.wfile.write(part)
                return
            super().do_GET()

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_port}'
    env = dict(os.environ, PATH='/nonexistent')

    def cli(path, *args, ok=True):
        p = subprocess.run([str(BIN), base + '/' + path, '--cache-dir', str(root / 'cache'), *map(str, args)],
                           env=env, capture_output=True, text=True)
        if (p.returncode == 0) != ok:
            raise AssertionError(f'{path}: exit {p.returncode}\n{p.stdout}\n{p.stderr}')
        return p

    def hashes(path, stream):
        # Decode both containers and compare all frame/sample content, ignoring timestamps.
        output = ff('-i', path, '-map', stream, '-f', 'framemd5', '-')
        return [line.split(b',')[-1].strip() for line in output.splitlines() if not line.startswith(b'#')]

    def same_media(path):
        # Compare decoded video frames and audio samples, ignoring container timestamps.
        for stream in ['0:v:0', '0:a:0']:
            assert hashes(path, stream) == hashes(web / 'original.ts', stream), f'Quality changed: {stream}'

    probe = cli('index.html', '--probe')
    assert '640x360' in probe.stderr and '320x180' in probe.stderr
    print('PASS webpage → iframe → master discovery and probe', flush=True)
    encrypted = root / 'encrypted.mp4'
    cli('index.html', '-o', encrypted, '--keep-cache')
    same_media(encrypted)
    assert hits['/enc0.ts'] == 2, 'Retry was not exercised'
    print('PASS AES-128 + retry + best selection + lossless MP4, PATH empty', flush=True)
    before = sum(hits[p] for p in ['/enc0.ts', '/enc1.ts', '/enc2.ts', '/key'])
    cli('index.html', '-o', root / 'resumed.mp4', '--keep-cache')
    after = sum(hits[p] for p in ['/enc0.ts', '/enc1.ts', '/enc2.ts', '/key'])
    assert before == after, 'Resume re-downloaded cached assets'
    print('PASS verified cache reuse', flush=True)
    cached = next((root / 'cache').rglob('*.bin'))
    cached.write_bytes(b'corrupted')
    cli('index.html', '-o', root / 'repaired.mp4', '--keep-cache')
    assert sum(hits[p] for p in ['/enc0.ts', '/enc1.ts', '/enc2.ts', '/key']) == after + 1
    print('PASS cache corruption detection and repair', flush=True)
    cli('index.html', '-q', '180p', '-o', root / 'limited.mp4')
    same_media(root / 'limited.mp4')
    cli('separate.m3u8', '-o', root / 'separate.mp4')
    same_media(root / 'separate.mp4')
    print('PASS height ceiling and external audio', flush=True)
    cli('range.m3u8', '-o', root / 'range.mp4')
    same_media(root / 'range.mp4')
    print('PASS fMP4 initialization + byte ranges', flush=True)
    cli('ambiguous.html', '-o', root / 'ambiguous.mp4', ok=False)
    cli('live.m3u8', '-o', root / 'live.mp4', ok=False)
    cli('live.m3u8', '--live-snapshot', '-o', root / 'live.mp4')
    cli('drm.m3u8', '-o', root / 'drm.mp4', ok=False)
    sentinel = root / 'exists.mp4'
    sentinel.write_bytes(b'do not replace')
    cli('clear.m3u8', '-o', sentinel, ok=False)
    assert sentinel.read_bytes() == b'do not replace'
    print('PASS ambiguous-source/live/DRM guards and output protection', flush=True)
    server.shutdown()
print('All integration checks passed.')
