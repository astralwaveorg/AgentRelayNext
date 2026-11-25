#!/usr/bin/env python3
"""Generate and verify the release asset set for one version.

Usage: verify-assets.py <dist-dir>

The release workflow must run this after building the app/ZIP/DMG, generating
the SBOM, and copying RELEASE_NOTES.md. It writes SHA256SUMS from an explicit
asset list (never a broad glob), verifies every line again, and asserts that
each DMG/ZIP has a sidecar and a manifest whose artifact name and hash match.
Exit code 0 only when the whole set is consistent.
"""
import hashlib
import json
import pathlib
import sys

def sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

def fail(message: str) -> None:
    print(json.dumps({"status": "failed", "reason": message}, ensure_ascii=False))
    sys.exit(1)

def read_digest(path: pathlib.Path) -> str:
    line = path.read_text().strip().split()
    if not line or len(line[0]) != 64:
        fail(f"malformed digest file: {path.name}")
    return line[0]

def check_manifest(path: pathlib.Path, artifact: pathlib.Path, digest: str) -> None:
    try:
        manifest = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        fail(f"invalid manifest {path.name}: {error}")
    if manifest.get("artifact") != artifact.name:
        fail(f"manifest artifact mismatch: {path.name}")
    if manifest.get("sha256") != digest:
        fail(f"manifest digest mismatch: {path.name}")
    if not manifest.get("product"):
        fail(f"manifest missing product: {path.name}")

def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: verify-assets.py <dist-dir>")
    dist = pathlib.Path(sys.argv[1])
    if not dist.is_dir():
        fail(f"not a directory: {dist}")

    names = sorted(p.name for p in dist.iterdir() if p.is_file())
    dmgs = [n for n in names if n.endswith(".dmg")]
    zips = [n for n in names if n.endswith(".zip") and not n.endswith(".sha256")]
    if len(dmgs) != 1:
        fail(f"expected exactly one DMG, found {dmgs}")
    if len(zips) != 1:
        fail(f"expected exactly one ZIP, found {zips}")

    assets: list[str] = []
    for kind, name in (("dmg", dmgs[0]), ("zip", zips[0])):
        artifact = dist / name
        digest = sha256(artifact)
        sidecar = dist / f"{name}.sha256"
        if not sidecar.is_file():
            fail(f"missing {sidecar.name}")
        if read_digest(sidecar) != digest:
            fail(f"sidecar digest mismatch: {sidecar.name}")
        manifest = dist / (
            f"{name}.manifest.json" if kind == "dmg" else f"{name[:-4]}.manifest.json"
        )
        if not manifest.is_file():
            fail(f"missing {manifest.name}")
        check_manifest(manifest, artifact, digest)
        assets.extend([name, sidecar.name, manifest.name])

    for required in ("AgentRelay-rust.spdx.json", "RELEASE_NOTES.md"):
        if not (dist / required).is_file():
            fail(f"missing {required}")
        assets.append(required)

    sums = dist / "SHA256SUMS"
    lines = [f"{sha256(dist / name)}  {name}\n" for name in sorted(set(assets))]
    sums.write_text("".join(lines))

    for name in [line.split("  ", 1)[1].strip() for line in lines]:
        if not (dist / name).is_file():
            fail(f"SHA256SUMS references a missing file: {name}")
    for line in sums.read_text().splitlines():
        digest, name = line.split("  ", 1)
        if sha256(dist / name.strip()) != digest:
            fail(f"SHA256SUMS verification failed: {name}")

    print(json.dumps({"status": "ok", "assets": sorted(set(assets)), "checksums": "SHA256SUMS"}))

if __name__ == "__main__":
    main()
