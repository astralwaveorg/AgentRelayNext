#!/usr/bin/env python3
"""Create an SPDX 2.3 inventory of the locked relayctl Cargo build closure."""
import hashlib
import json
import re
import sys
from datetime import datetime, timezone
from urllib.parse import quote

metadata = json.load(sys.stdin)
packages = {p["id"]: p for p in metadata["packages"]}
nodes = {n["id"]: n["deps"] for n in metadata["resolve"]["nodes"]}
root, = (
    p["id"]
    for p in packages.values()
    if p["name"] == "relay-daemon" and p["id"] in metadata["workspace_members"]
)
closure = set()
pending = [root]
while pending:
    pkg = pending.pop()
    if pkg not in closure:
        closure.add(pkg)
        # Include normal and build dependencies, but not test-only dependencies.
        pending.extend(dep["pkg"] for dep in nodes[pkg]
                       if any(k["kind"] != "dev" for k in dep["dep_kinds"]))

def spdx_id(pkg):
    p = packages[pkg]
    source = p["source"] or "workspace:" + p["name"]
    suffix = hashlib.sha256(source.encode()).hexdigest()[:12]
    return "SPDXRef-" + re.sub(r"[^A-Za-z0-9.-]", "-", f'{p["name"]}-{p["version"]}-{suffix}')

def entry(pkg):
    p = packages[pkg]
    source = p["source"]
    result = {
        "SPDXID": spdx_id(pkg), "name": p["name"], "versionInfo": p["version"],
        "downloadLocation": "NOASSERTION", "filesAnalyzed": False,
        "licenseDeclared": p["license"] or "NOASSERTION",
        "licenseConcluded": "NOASSERTION", "copyrightText": "NOASSERTION",
    }
    if source in {"registry+https://github.com/rust-lang/crates.io-index", "registry+https://index.crates.io/"}:
        name, version = quote(p["name"], safe=""), quote(p["version"], safe="")
        result["downloadLocation"] = f"https://crates.io/api/v1/crates/{name}/{version}/download"
        result["externalRefs"] = [{
            "referenceCategory": "PACKAGE-MANAGER",
            "referenceType": "purl", "referenceLocator": f"pkg:cargo/{name}@{version}",
        }]
    return result

ordered = sorted(closure, key=spdx_id)
relationships = {
    (spdx_id(p), spdx_id(d["pkg"]))
    for p in ordered for d in nodes[p]
    if any(k["kind"] != "dev" for k in d["dep_kinds"])
}
entries = [entry(p) for p in ordered]
links = [{"spdxElementId": a, "relationshipType": "DEPENDS_ON", "relatedSpdxElement": b}
         for a, b in sorted(relationships)]
version = packages[root]["version"]
digest = hashlib.sha256(json.dumps([entries, links], sort_keys=True).encode()).hexdigest()
report = {
    "SPDXID": "SPDXRef-DOCUMENT",
    "spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0",
    "name": f"AgentRelay-relayctl-{version}-arm64-build-dependencies",
    "documentNamespace": f"https://spdx.org/spdxdocs/agent-relay-{version}-{digest}",
    "creationInfo": {
        "created": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "creators": ["Tool: AgentRelay Cargo inventory"],
    },
    "comment": ("Only the locked Cargo build graph for relayctl on aarch64 macOS, "
                "including build dependencies. Bundle resources, the Swift app, platform libraries, "
                "and source vendored inside a crate need separate inventory and license review."),
    "documentDescribes": [spdx_id(root)], "packages": entries, "relationships": links,
}
json.dump(report, sys.stdout, ensure_ascii=False, sort_keys=True, indent=2)
print()
