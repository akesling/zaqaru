"""Build the same fixture against the actual pre-experiment engine sources."""
from pathlib import Path
import re
import subprocess
import tomllib

baseline = "faf6988"
root = Path("benchmark-results/specialize-reference")
root.mkdir(parents=True, exist_ok=True)
archive = subprocess.check_output(["git", "archive", baseline, "Cargo.toml", "Cargo.lock", "crates/cpu", "crates/x87"])
subprocess.run(["tar", "-x", "-C", str(root)], input=archive, check=True)
manifest = root / "Cargo.toml"
manifest.write_text(re.sub(r"members = \[.*?\]", 'members = ["crates/cpu", "crates/x87"]', manifest.read_text(), count=1, flags=re.S))
manifest = root / "crates/cpu/Cargo.toml"
text = manifest.read_text().replace("[features]", "[features]\nspecialize = []")
text += '\n[[example]]\nname = "specialize"\ncrate-type = ["cdylib"]\nrequired-features = ["specialize"]\n'
manifest.write_text(text)
fixture = Path("crates/cpu/examples/specialize.rs").read_text()
start = fixture.index('unsafe extern "C" fn generic(')
end = fixture.index("\nfn leave_code", start)
# The old engine has no specialization entry point. Its mode-zero run and all
# fixture construction/state checks remain identical. Wizer registers a dummy
# request, which is never executed by the reference harness.
fixture = fixture[:start] + '''#[allow(unused_variables)]
unsafe extern "C" fn generic(code: *const u64, len: u32, ip: *const u64,
    tcb: *mut Tcb, space: *mut Space, budget: u64) -> u32 {
    unreachable!("the reference engine is never run in specialized mode")
}
''' + fixture[end:]
(root / "crates/cpu/examples/specialize.rs").write_text(fixture)

# Trimming the workspace requires pruning its lockfile. Stay offline and reject
# any dependency/version/checksum not present in the historical lockfile.
lock = root / "Cargo.lock"
def packages():
    return {(p["name"], p["version"], p.get("checksum")) for p in tomllib.loads(lock.read_text())["package"]}
before = packages()
subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1", "--manifest-path", str(root / "Cargo.toml")], check=True, stdout=subprocess.DEVNULL)
assert packages() <= before, "Reference dependencies changed from the historical lockfile"
