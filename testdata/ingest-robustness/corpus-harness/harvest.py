#!/usr/bin/env python3
"""Harvest a real-world EDA-file corpus from GitHub into ingest-corpus/.

Two harvest routes:
  * code search (`search/code`) for text formats GitHub indexes
  * git-tree scans of candidate repos for binary formats GitHub does not index

Every downloaded file gets a manifest row: repo, ref (commit), path, sha, bytes.
"""
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(ROOT, "ingest-corpus")
MANIFEST = os.path.join(CORPUS, "manifest.jsonl")

_seen_sha = set()
_manifest_rows = []


def load_manifest():
    if os.path.exists(MANIFEST):
        with open(MANIFEST) as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                row = json.loads(line)
                _manifest_rows.append(row)
                _seen_sha.add(row["sha"])


def gh(path, params=None, retries=4):
    cmd = ["gh", "api", "-X", "GET", path]
    for k, v in (params or {}).items():
        cmd += ["-f", f"{k}={v}"]
    for attempt in range(retries):
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode == 0:
            try:
                return json.loads(p.stdout)
            except json.JSONDecodeError:
                return None
        err = (p.stderr or "")[:300]
        if "rate limit" in err.lower() or "403" in err or "was submitted too quickly" in err:
            wait = 20 * (attempt + 1)
            print(f"  [rate-limit] sleeping {wait}s ({err[:80]})", flush=True)
            time.sleep(wait)
            continue
        print(f"  [gh error] {path} {params}: {err[:160]}", flush=True)
        return None
    return None


def code_search(query, pages=3, per_page=100):
    """Yield search/code items. Code search allows 10 req/min."""
    out = []
    for page in range(1, pages + 1):
        res = gh("search/code", {"q": query, "per_page": per_page, "page": page})
        time.sleep(7)  # stay under 10/min
        if not res or not res.get("items"):
            break
        out.extend(res["items"])
        if len(res["items"]) < per_page:
            break
    return out


def ref_of(item):
    url = item.get("url", "")
    m = re.search(r"[?&]ref=([0-9a-f]{7,40})", url)
    return m.group(1) if m else "HEAD"


def raw_url(repo, ref, path):
    return "https://raw.githubusercontent.com/{}/{}/{}".format(
        repo, ref, urllib.parse.quote(path)
    )


def download(bucket, repo, ref, path, sha, max_mb=40):
    """Download one file into CORPUS/bucket/. Returns manifest row or None."""
    if sha in _seen_sha:
        return None
    dest_dir = os.path.join(CORPUS, bucket)
    os.makedirs(dest_dir, exist_ok=True)
    safe = (repo.replace("/", "__") + "__" + path.replace("/", "__"))[-160:]
    dest = os.path.join(dest_dir, safe)
    url = raw_url(repo, ref, path)
    p = subprocess.run(
        ["curl", "-sSL", "--max-time", "120", "--max-filesize", str(max_mb * 1024 * 1024),
         "-o", dest, url],
        capture_output=True, text=True,
    )
    if p.returncode != 0 or not os.path.exists(dest) or os.path.getsize(dest) == 0:
        if os.path.exists(dest):
            os.remove(dest)
        return None
    _seen_sha.add(sha)
    return {
        "bucket": bucket,
        "file": os.path.relpath(dest, CORPUS),
        "repo": repo,
        "ref": ref,
        "path": path,
        "sha": sha,
        "bytes": os.path.getsize(dest),
        "url": f"https://github.com/{repo}/blob/{ref}/{urllib.parse.quote(path)}",
    }


def harvest_code(bucket, query, pages=3, max_mb=40):
    items = code_search(query, pages=pages)
    print(f"[{bucket}] query {query!r} -> {len(items)} hits", flush=True)
    jobs = []
    for it in items:
        repo = it["repository"]["full_name"]
        jobs.append((bucket, repo, ref_of(it), it["path"], it["sha"], max_mb))
    got = 0
    with ThreadPoolExecutor(max_workers=8) as ex:
        for row in ex.map(lambda a: download(*a), jobs):
            if row:
                _manifest_rows.append(row)
                got += 1
    print(f"[{bucket}]   downloaded {got}", flush=True)
    flush_manifest()
    return got


def tree_scan(repo, exts, max_files=40):
    """List files in a repo matching extensions, via the recursive tree API."""
    info = gh(f"repos/{repo}")
    if not info:
        return []
    branch = info.get("default_branch", "main")
    tree = gh(f"repos/{repo}/git/trees/{branch}", {"recursive": "1"})
    if not tree or "tree" not in tree:
        return []
    ref = branch
    out = []
    for node in tree["tree"]:
        if node.get("type") != "blob":
            continue
        p = node["path"]
        low = p.lower()
        if any(low.endswith(e.lower()) for e in exts):
            out.append((repo, ref, p, node["sha"], node.get("size", 0)))
        if len(out) >= max_files:
            break
    return out


def harvest_tree(bucket, repos, exts, max_files=40, max_mb=40):
    jobs = []
    for repo in repos:
        found = tree_scan(repo, exts, max_files=max_files)
        print(f"[{bucket}] tree {repo} -> {len(found)}", flush=True)
        for repo_, ref, path, sha, _sz in found:
            jobs.append((bucket, repo_, ref, path, sha, max_mb))
    got = 0
    with ThreadPoolExecutor(max_workers=8) as ex:
        for row in ex.map(lambda a: download(*a), jobs):
            if row:
                _manifest_rows.append(row)
                got += 1
    print(f"[{bucket}]   downloaded {got}", flush=True)
    flush_manifest()
    return got


def repo_search(query, pages=2, per_page=50):
    names = []
    for page in range(1, pages + 1):
        res = gh("search/repositories", {"q": query, "per_page": per_page, "page": page})
        time.sleep(2)
        if not res or not res.get("items"):
            break
        names += [i["full_name"] for i in res["items"]]
    return names


def flush_manifest():
    os.makedirs(CORPUS, exist_ok=True)
    with open(MANIFEST, "w") as fh:
        for row in _manifest_rows:
            fh.write(json.dumps(row) + "\n")


if __name__ == "__main__":
    load_manifest()
    print(f"manifest starts with {len(_manifest_rows)} rows", flush=True)
    stage = sys.argv[1] if len(sys.argv) > 1 else "all"

    if stage in ("all", "kicad"):
        # Version-targeted queries so the corpus spans KiCad 4 through 10.
        for ver in ["20171130", "20171114", "20211014", "20221018", "20240108",
                    "20241229", "20230121", "20260206", "20250213", "20221018"]:
            harvest_code("kicad_pcb", f'"(version {ver})" extension:kicad_pcb', pages=2)
        harvest_code("kicad_pcb", "extension:kicad_pcb kicad_pcb", pages=3)
        harvest_code("kicad_pcb", "extension:kicad_pcb footprint", pages=3)
        harvest_code("kicad_pcb", "extension:kicad_pcb module", pages=2)

    if stage in ("all", "sch"):
        harvest_code("kicad_sch", "extension:kicad_sch symbol", pages=2)
        harvest_code("kicad_net", "extension:net export tool kicad", pages=2)

    if stage in ("all", "eagle"):
        harvest_code("eagle_brd", "extension:brd eagle version", pages=3)
        harvest_code("eagle_brd", '"<!DOCTYPE eagle SYSTEM" extension:brd', pages=2)

    if stage in ("all", "altium"):
        harvest_code("altium", "extension:PcbDoc", pages=3)
        harvest_code("altium", "extension:pcbdoc", pages=2)

    if stage in ("all", "d356"):
        harvest_code("ipc356", "extension:d356", pages=3)
        harvest_code("ipc356", "extension:ipc", pages=1)

    if stage in ("all", "gerber"):
        harvest_code("gerber_loose", "extension:gbr G04", pages=3)
        harvest_code("gerber_loose", "extension:gtl", pages=2)
        harvest_code("gerber_loose", "extension:drl M48", pages=2)

    flush_manifest()
    print(f"manifest now {len(_manifest_rows)} rows", flush=True)
