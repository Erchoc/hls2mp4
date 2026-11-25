#!/usr/bin/env python3
"""Build-time only: embed FFmpeg and its non-system macOS libraries.
On Linux/Windows pass a statically linked executable with --ffmpeg.
Only package binaries whose redistribution licenses you can satisfy.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--ffmpeg', default=shutil.which('ffmpeg'))
a = p.parse_args()
if not a.ffmpeg:
    p.error('FFmpeg is required on the build machine')
root = Path(__file__).resolve().parents[1]
(root / 'assets').mkdir(exist_ok=True)
with tempfile.TemporaryDirectory() as d:
    stage = Path(d)
    executable = stage / ('ffmpeg.exe' if sys.platform == 'win32' else 'ffmpeg')
    shutil.copy2(a.ffmpeg, executable)
    executable.chmod(0o755)
    if sys.platform == 'darwin':
        seen = {}
        def deps(path):
            lines = subprocess.check_output(['otool', '-L', str(path)], text=True).splitlines()[1:]
            return [line.strip().split(' (')[0] for line in lines]
        def collect(source, target):
            changes = []
            for dep in deps(source):
                if dep.startswith(('/usr/lib/', '/System/')):
                    continue
                if dep.startswith('@'):
                    raise RuntimeError(f'Unresolved install name {dep} in {source}; use absolute library paths')
                dep_path = Path(dep)
                real = dep_path.resolve()
                name = dep_path.name
                if name in seen and seen[name] != real:
                    raise RuntimeError(f'Library name collision: {name}')
                if name not in seen:
                    seen[name] = real
                    dest = stage / name
                    shutil.copy2(real, dest)
                    dest.chmod(0o755)
                    collect(real, dest)
                    subprocess.run(['install_name_tool', '-id', '@loader_path/' + name, str(dest)], check=True)
                changes += ['-change', dep, '@loader_path/' + name]
            if changes:
                subprocess.run(['install_name_tool', *changes, str(target)], check=True)
        collect(Path(a.ffmpeg).resolve(), executable)
        for binary in stage.iterdir():
            subprocess.run(['codesign', '--force', '--sign', '-', str(binary)], check=True, capture_output=True)
    elif sys.platform.startswith('linux'):
        result = subprocess.run(['ldd', str(executable)], capture_output=True, text=True)
        if '=>' in result.stdout:
            raise RuntimeError('Use a statically linked FFmpeg for Linux')
    subprocess.run([str(executable), '-version'], check=True, stdout=subprocess.DEVNULL)
    archive = root / 'assets' / 'ffmpeg.tar.gz'
    with tarfile.open(archive, 'w:gz') as tar:
        for binary in sorted(stage.iterdir()):
            tar.add(binary, arcname=binary.name)
    print(f'Bundled {len(list(stage.iterdir()))} files, {archive.stat().st_size / 1048576:.1f} MiB: {archive}')
