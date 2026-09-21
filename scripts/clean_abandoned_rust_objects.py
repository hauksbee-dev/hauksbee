#!/usr/bin/env python3
"""Remove loose rustc linker inputs, not libraries, fingerprints or caches.

Local Unix development only. Cargo's advisory locks prevent new coordinated
builds; a process check also refuses cleanup around orphaned compiler/linkers.
Do not start an out-of-band rustc while this runs. No shell globs or cargo clean.
"""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import time


class Busy(RuntimeError):
    pass


def require_idle():
    output = subprocess.check_output(['ps', '-axo', 'comm='], text=True)
    active = sorted({Path(line.strip()).name for line in output.splitlines()} & {
        'cargo', 'rustc', 'clippy-driver', 'rust-lld', 'ld', 'ld.lld', 'cc', 'clang',
    })
    if active:
        raise Busy('compiler/build processes are active: ' + ', '.join(active))


def clean(root, apply=False):
    import fcntl  # Unix-only; main checks the platform before entering.

    root = Path(root).resolve()
    profile = root / 'target/.build/debug'
    deps = profile / 'deps'
    result = {'applied': apply, 'matched': 0, 'removed': 0, 'bytes_unlinked': 0,
              'other_entries': 0, 'recent_entries': 0, 'path': str(deps)}
    if not deps.exists():
        return result
    for path in (deps, root / 'target/debug', profile):
        if path.resolve() != path:
            raise RuntimeError(f'refusing redirected build path: {path}')
    started = time.monotonic()
    names = hashlib.sha256()
    with ExitStack() as stack:
        # Cargo can lock either artifact location with build-dir enabled.
        for path in sorted({profile / '.cargo-lock', root / 'target/debug/.cargo-lock'}):
            if path.is_symlink() or not path.is_file():
                raise RuntimeError(f'missing or redirected Cargo lock: {path}')
            fd = os.open(path, os.O_RDWR | os.O_NOFOLLOW)
            lock = stack.enter_context(os.fdopen(fd, 'r+'))
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise Busy(f'Cargo owns {path}') from None
        require_idle()
        directory = os.open(deps, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        stack.callback(os.close, directory)
        cutoff = time.time()
        with os.scandir(directory) as entries:
            for entry in entries:
                if not entry.name.endswith('.rcgu.o') or not entry.is_file(follow_symlinks=False):
                    result['other_entries'] += 1
                    continue
                info = os.stat(entry.name, dir_fd=directory, follow_symlinks=False)
                if not stat.S_ISREG(info.st_mode):
                    continue
                if info.st_mtime >= cutoff:
                    result['recent_entries'] += 1
                    continue
                result['matched'] += 1
                if result['matched'] % 10000 == 0:
                    require_idle()
                if apply:
                    os.unlink(entry.name, dir_fd=directory)
                    result['removed'] += 1
                    result['bytes_unlinked'] += info.st_size
                    names.update(os.fsencode(entry.name) + b'\0')
        result['removed_names_sha256'] = names.hexdigest()
        result['seconds'] = round(time.monotonic() - started, 3)
        if result['removed']:
            receipts = root / 'target/build-cleanup'
            receipts.mkdir(exist_ok=True)
            (receipts / f'{time.time_ns()}.json').write_text(json.dumps(result, indent=2) + '\n')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true', help='delete abandoned objects; otherwise count only')
    args = parser.parse_args()
    if os.name != 'posix':
        print('Rust object cleanup skipped: local Unix builds only.')
        return
    if any(os.environ.get(key) for key in ('CARGO_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR')):
        print('Rust object cleanup skipped: custom Cargo directories require manual review.')
        return
    flags = os.environ.get('RUSTFLAGS', '') + os.environ.get('CARGO_ENCODED_RUSTFLAGS', '')
    if 'save-temps' in flags:
        print('Rust object cleanup skipped: save-temps was explicitly requested.')
        return
    try:
        print(json.dumps(clean(Path(__file__).resolve().parents[1], args.apply)))
    except Busy as error:
        print(f'Rust object cleanup skipped: {error}')
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        parser.exit(1, f'Rust object cleanup refused: {error}\n')


if __name__ == '__main__':
    main()
