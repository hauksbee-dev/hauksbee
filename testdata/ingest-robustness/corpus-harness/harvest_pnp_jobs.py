#!/usr/bin/env python3
"""Harvest gerber fab folders that DO ship a pick-and-place file.

The plain gerber-job harvest showed that published fab folders almost never
include one, so the success path (copper plus a part list, which is the only way
a gerber job can bind) needs its own targeted search: find the P&P file first,
then pull the whole directory it lives in.
"""
import os
import subprocess
import sys
import zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harvest as H
from harvest_gerber_jobs import FAB_EXT, job_files

CORPUS = H.CORPUS
PNP_EXT = (".pos", ".csv", ".txt")

QUERIES = [
    'extension:pos "Ref" "PosX"',
    'extension:pos "### Module positions"',
    'extension:csv "Designator" "Mid X"',
    'extension:csv "Ref,Val,Package,PosX"',
    'extension:csv "Designator,Val,Package,Center-X"',
]


def main():
    H.load_manifest()
    cands = {}
    for q in QUERIES:
        for it in H.code_search(q, pages=2):
            repo = it["repository"]["full_name"]
            cands[(repo, H.ref_of(it), os.path.dirname(it["path"]))] = it["path"]
        print(f"  after {q!r}: {len(cands)} dirs", flush=True)

    made = 0
    for (repo, ref, directory), pnp_path in cands.items():
        if made >= 25:
            break
        files = job_files(repo, ref, directory)
        pnp = [f for f in files if f[0] == pnp_path]
        if not pnp:
            files = files + [(pnp_path, "pnp", 0)]
        copper_ish = [
            f for f in files
            if f[0].lower().endswith((".gbr", ".gtl", ".gbl", ".ger", ".art", ".cmp", ".sol"))
        ]
        if len(copper_ish) < 2:
            continue
        job = (repo.replace("/", "__") + "__" + (directory.replace("/", "__") or "root"))[-100:]
        jobdir = os.path.join(CORPUS, "gerber_pnp", job)
        os.makedirs(jobdir, exist_ok=True)
        rows = []
        for path, sha, _sz in files[:40]:
            dest = os.path.join(jobdir, os.path.basename(path))
            p = subprocess.run(
                ["curl", "-sSL", "--max-time", "90", "--max-filesize", str(20 * 1024 * 1024),
                 "-o", dest, H.raw_url(repo, ref, path)], capture_output=True)
            if p.returncode != 0 or not os.path.exists(dest) or os.path.getsize(dest) == 0:
                continue
            rows.append({
                "bucket": "gerber_pnp", "file": os.path.relpath(dest, CORPUS),
                "repo": repo, "ref": ref, "path": path, "sha": sha,
                "bytes": os.path.getsize(dest),
                "url": f"https://github.com/{repo}/blob/{ref}/{path}",
            })
        if len(rows) < 3 or not any(r["path"] == pnp_path for r in rows):
            continue
        H._manifest_rows.extend(rows)
        zdir = os.path.join(CORPUS, "gerber_zip")
        os.makedirs(zdir, exist_ok=True)
        zpath = os.path.join(zdir, job + "__pnp.zip")
        with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
            for fn in sorted(os.listdir(jobdir)):
                z.write(os.path.join(jobdir, fn), fn)
        H._manifest_rows.append({
            "bucket": "gerber_zip", "file": os.path.relpath(zpath, CORPUS),
            "repo": repo, "ref": ref, "path": directory, "sha": "pnp-zip-of:" + job,
            "bytes": os.path.getsize(zpath),
            "url": f"https://github.com/{repo}/tree/{ref}/{directory}",
            "note": "locally zipped fab folder that includes a pick-and-place file",
        })
        made += 1
        print(f"  pnp job {made}: {job} ({len(rows)} files)", flush=True)
        H.flush_manifest()
    H.flush_manifest()
    print(f"built {made} pick-and-place gerber jobs", flush=True)


if __name__ == "__main__":
    main()
