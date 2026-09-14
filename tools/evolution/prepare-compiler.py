#!/usr/bin/env python3
"""Build a reproducible browser-only weval library from the pinned checkout.

No upstream sources are modified. Generated sources and their original license
live under benchmark-results; this is deliberately outside normal builds/CI.
"""
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "benchmark-results/weval-source"
DEST = ROOT / "benchmark-results/browser-compiler"
REVISION = "04191f69e9cdf624a272be01887dfbe5cf306bfa"
assert subprocess.check_output(["git", "-C", str(SOURCE), "rev-parse", "HEAD"], text=True).strip() == REVISION
assert not subprocess.check_output(["git", "-C", str(SOURCE), "status", "--porcelain"], text=True).strip()
(DEST / "src").mkdir(parents=True, exist_ok=True)
modules = "constant_offsets dce directive escape eval image intrinsics liveness state stats value".split()
for module in modules:
    content = (SOURCE / f"src/{module}.rs").read_text()
    content = content.replace("use rayon::prelude::*;", "")
    if module == "image":
        content = content.replace("use rayon::iter::{IntoParallelIterator, ParallelExtend, ParallelIterator};", "")
        content = content.replace(".par_extend(", ".extend(").replace(".into_par_iter()", ".into_iter()")
    if module == "eval":
        assert content.count(".par_iter()") == 1
        content = content.replace("use rayon::prelude::*;", "")
        content = content.replace(".par_iter()", ".iter()")
        content = content.replace("indicatif::ProgressBar", "crate::Progress")
        # Preserve the evaluation-limit reason through the Block's warning
        # channel, without dumping the entire frozen argument buffer.
        old = 'log::info!(\n                    " -> too many blocks or values:'
        assert content.count(old) == 1
        content = content.replace(old, 'log::warn!(\n                    " -> too many blocks or values:')
        content = content.replace('log::warn!("Failed to weval for directive {directive:?}");',
                                  'log::warn!("Failed to weval for directive {}", directive.user_id);')
    (DEST / f"src/{module}.rs").write_text(content)
for license in SOURCE.glob("LICENSE*"):
    shutil.copyfile(license, DEST / license.name)
(DEST / "src/cache.rs").write_text('''
#[derive(Clone, Debug)]
pub struct CacheData { pub sig: u32, pub name: String, pub body: Vec<u8> }
pub struct Cache;
impl Cache {
    pub fn can_insert(&self) -> bool { false }
    pub fn thread(&self) -> anyhow::Result<Self> { Ok(Self) }
    pub fn lookup(&mut self, _: &[u8]) -> anyhow::Result<Option<CacheData>> { Ok(None) }
    pub fn insert(&mut self, _: &[u8], _: CacheData) -> anyhow::Result<()> { unreachable!() }
}
''')
shutil.copyfile(ROOT / "tools/evolution/compiler-block.rs", DEST / "src/lib.rs")
shutil.copyfile(ROOT / "tools/evolution/src/artifact.rs", DEST / "src/artifact.rs")
(DEST / "Cargo.toml").write_text('''
[package]
name = "zaqaru-browser-compiler"
version = "0.1.0"
edition = "2021"
publish = false
[workspace]
[lib]
crate-type = ["cdylib"]
[dependencies]
waffle = "=0.2.0"
anyhow = "1"
log = "0.4"
fxhash = "0.2"
bincode = "1.3.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
base64 = "0.22"
wasm-encoder = { version = "=0.254.0", features = ["wasmparser"] }
wasmparser = "=0.254.0"
featherweight-guest = { path = "../structfs/featherweight/guest", default-features = false }
[profile.release]
panic = "abort"
''')
print(DEST)

lock = ROOT / "tools/evolution/compiler-Cargo.lock"
if lock.exists():
    shutil.copyfile(lock, DEST / "Cargo.lock")
