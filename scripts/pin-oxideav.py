#!/usr/bin/env python3
"""Pins every OxideAV crate this workspace uses, directly or through another
OxideAV crate, to the git revision checked out in a local OxideAV workspace
clone, and rewrites the [patch.crates-io] block of the root Cargo.toml.

OxideAV's crates.io releases lag its repositories, so the whole graph comes
from git. Run after `./scripts/update-crates.sh` in the clone:

    python3 scripts/pin-oxideav.py ~/projects/oxideav

A crate forked to ayooooo123/oxideav-<name> is pinned to the fork when the
clone's `origin` points there.
"""
import pathlib, re, subprocess, sys, tomllib

root = pathlib.Path(__file__).resolve().parent.parent
clone = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "~/projects/oxideav").expanduser() / "crates"

def oxideav_deps(manifest):
    data = tomllib.loads(manifest.read_text())
    names = set()
    for table in ("dependencies", "dev-dependencies", "build-dependencies"):
        names |= {n for n in data.get(table, {}) if n.startswith("oxideav-")}
    for target in data.get("target", {}).values():
        names |= {n for n in target.get("dependencies", {}) if n.startswith("oxideav-")}
    return names

# Roots: OxideAV crates named by this workspace's member manifests.
wanted = set()
for manifest in root.glob("crates/*/Cargo.toml"):
    wanted |= oxideav_deps(manifest)
wanted |= {n for n in re.findall(r'^(oxideav-[a-z0-9-]+)\s*=', (root / "Cargo.toml").read_text(), re.M)}

# Close over OxideAV's own dependencies (dev-dependencies excluded there).
pending, seen = list(wanted), set()
while pending:
    name = pending.pop()
    if name in seen:
        continue
    seen.add(name)
    manifest = clone / name / "Cargo.toml"
    if not manifest.is_file():
        sys.exit(f"{name}: not in {clone}; run update-crates.sh there")
    data = tomllib.loads(manifest.read_text())
    deps = {n for n in data.get("dependencies", {}) if n.startswith("oxideav-")}
    for target in data.get("target", {}).values():
        deps |= {n for n in target.get("dependencies", {}) if n.startswith("oxideav-")}
    pending.extend(deps - seen)

lines = ["[patch.crates-io]"]
for name in sorted(seen):
    repo = clone / name
    rev = subprocess.check_output(["git", "-C", repo, "rev-parse", "HEAD"], text=True).strip()
    url = subprocess.check_output(["git", "-C", repo, "remote", "get-url", "origin"], text=True).strip()
    url = url.removesuffix(".git")
    lines.append(f'{name} = {{ git = "{url}", rev = "{rev}" }}')
block = "\n".join(lines) + "\n"

cargo = (root / "Cargo.toml").read_text()
marked = "# BEGIN pin-oxideav\n" + block + "# END pin-oxideav\n"
pattern = re.compile(r"# BEGIN pin-oxideav\n.*?# END pin-oxideav\n", re.S)
cargo = pattern.sub(lambda _: marked, cargo) if pattern.search(cargo) else cargo.rstrip("\n") + "\n\n" + marked
(root / "Cargo.toml").write_text(cargo)
print(f"pinned {len(seen)} OxideAV crates")
