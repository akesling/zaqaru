#!/usr/bin/env python3
"""Prepare pinned, ignored dependencies. Never changes either upstream repo."""
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[2]
results = root / "benchmark-results"
results.mkdir(exist_ok=True)
baseline = results / 'evolution-baseline-source'
baseline.mkdir(exist_ok=True)
subprocess.run(['tar', '-x', '-C', str(baseline)], input=subprocess.check_output(['git', '-C', str(root), 'archive', 'faf6988']), check=True)
structfs = Path(sys.argv[1]) if len(sys.argv) > 1 else Path('/Users/alex/Devel/AdjectiveNoun/structfs')
revision = '81b37c51e9e400c214571b40632c8f675448f3f9'
archive = subprocess.check_output(['git', '-C', str(structfs), 'archive', revision])
target = results / 'structfs'
target.mkdir(exist_ok=True)
subprocess.run(['tar', '-x', '-C', str(target)], input=archive, check=True)
weval = results / 'weval-source'
if not weval.exists():
    subprocess.run(['git', 'clone', '--depth', '1', '--branch', 'v0.5.0',
                    'https://github.com/bytecodealliance/weval.git', str(weval)], check=True)
subprocess.run([sys.executable, str(root / 'tools/evolution/prepare-compiler.py')], check=True)
subprocess.run(['npm', 'ci', '--prefix', str(target / 'featherweight/host/browser')], check=True)
subprocess.run(['npm', 'run', 'build', '--prefix', str(target / 'featherweight/host/browser')], check=True)
