#!/usr/bin/env python3
"""Assemble whole gerber JOBS (a fab folder, not one loose layer) from GitHub.

A single `.gbr` is not a board, so the loose-gerber bucket only tests refusal.
This walks the repos that produced those hits, pulls every sibling fab file out
of the same directory via the git-tree API, and writes each job twice:

  gerber_dir/<job>/...      the unzipped fab folder
  gerber_zip/<job>.zip      the same folder zipped, the form users upload

Provenance for every file lands in the shared manifest.
"""
import json
import os
import sys
import zipfile
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harvest as H

FAB_EXT = (
    ".gbr", ".gbl", ".gtl", ".gbs", ".gts", ".gbo", ".gto", ".gbp", ".gtp",
    ".gml", ".gko", ".gm1", ".gm13", ".g1", ".g2", ".g3", ".g4",
    ".drl", ".txt", ".xln", ".nc", ".tap", ".gbrjob", ".ger",
    ".cmp", ".sol", ".plc", ".pls", ".stc", ".sts", ".drd", ".dri",
)
CORPUS = H.CORPUS


def dirs_from_search():
    """Directories that contain gerber-looking files, from code search."""
    queries = [
        "extension:gbr G04",
        'extension:gbr "%TF.FileFunction"',
        "extension:gbrjob GenerationSoftware",
        "extension:gtl G54",
        "extension:GTL",
        "extension:drl M48",
    ]
    found = {}
    for q in queries:
        for it in H.code_search(q, pages=2):
            repo = it["repository"]["full_name"]
            ref = H.ref_of(it)
            d = os.path.dirname(it["path"])
            found[(repo, ref, d)] = True
        print(f"  after {q!r}: {len(found)} candidate dirs", flush=True)
    return list(found)


def job_files(repo, ref, directory):
    tree = H.gh(f"repos/{repo}/git/trees/{ref}", {"recursive": "1"})
    if not tree or "tree" not in tree:
        return []
    out = []
    for node in tree["tree"]:
        if node.get("type") != "blob":
            continue
        p = node["path"]
        if os.path.dirname(p) != directory:
            continue
        if p.lower().endswith(FAB_EXT):
            out.append((p, node["sha"], node.get("size", 0)))
    return out


def main():
    H.load_manifest()
    cands = dirs_from_search()
    print(f"{len(cands)} candidate gerber directories", flush=True)
    made = 0
    for repo, ref, directory in cands:
        if made >= 60:
            break
        files = job_files(repo, ref, directory)
        # A real fab job has several layers; 1-2 files is a stray export.
        if len(files) < 4:
            continue
        job = (repo.replace("/", "__") + "__" + (directory.replace("/", "__") or "root"))[-100:]
        jobdir = os.path.join(CORPUS, "gerber_dir", job)
        os.makedirs(jobdir, exist_ok=True)
        rows = []
        for path, sha, _sz in files[:40]:
            url = H.raw_url(repo, ref, path)
            dest = os.path.join(jobdir, os.path.basename(path))
            import subprocess
            p = subprocess.run(
                ["curl", "-sSL", "--max-time", "90", "--max-filesize", str(20 * 1024 * 1024),
                 "-o", dest, url], capture_output=True)
            if p.returncode != 0 or not os.path.exists(dest) or os.path.getsize(dest) == 0:
                continue
            rows.append({
                "bucket": "gerber_dir", "file": os.path.relpath(dest, CORPUS),
                "repo": repo, "ref": ref, "path": path, "sha": sha,
                "bytes": os.path.getsize(dest),
                "url": f"https://github.com/{repo}/blob/{ref}/{path}",
            })
        if len(rows) < 4:
            continue
        H._manifest_rows.extend(rows)
        # The zipped form users actually upload.
        zdir = os.path.join(CORPUS, "gerber_zip")
        os.makedirs(zdir, exist_ok=True)
        zpath = os.path.join(zdir, job + ".zip")
        with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
            for fn in sorted(os.listdir(jobdir)):
                z.write(os.path.join(jobdir, fn), fn)
        H._manifest_rows.append({
            "bucket": "gerber_zip", "file": os.path.relpath(zpath, CORPUS),
            "repo": repo, "ref": ref, "path": directory, "sha": "zip-of:" + job,
            "bytes": os.path.getsize(zpath),
            "url": f"https://github.com/{repo}/tree/{ref}/{directory}",
            "note": "locally zipped from the fab folder above",
        })
        # A nested-folder zip too: the shape a user gets from zipping the parent.
        zpath2 = os.path.join(zdir, job + "__nested.zip")
        with zipfile.ZipFile(zpath2, "w", zipfile.ZIP_DEFLATED) as z:
            for fn in sorted(os.listdir(jobdir)):
                z.write(os.path.join(jobdir, fn), os.path.join("gerbers", fn))
        H._manifest_rows.append({
            "bucket": "gerber_zip", "file": os.path.relpath(zpath2, CORPUS),
            "repo": repo, "ref": ref, "path": directory,
            "sha": "nested-zip-of:" + job,
            "bytes": os.path.getsize(zpath2),
            "url": f"https://github.com/{repo}/tree/{ref}/{directory}",
            "note": "locally zipped with the files one folder deep",
        })
        made += 1
        print(f"  job {made}: {job} ({len(rows)} files)", flush=True)
        H.flush_manifest()
    H.flush_manifest()
    print(f"built {made} gerber jobs", flush=True)


if __name__ == "__main__":
    main()
