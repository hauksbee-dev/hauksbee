#!/usr/bin/env python3
"""Validate simulator archives and bind executables to one verified tree."""

from __future__ import annotations

import argparse
import os
import stat
import sys
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


class UnsafeArchive(ValueError):
    """An archive member can address data outside the extraction root."""


def _is_absolute(name: str) -> bool:
    return (
        PurePosixPath(name).is_absolute()
        or name.startswith("\\")
        or (len(name) >= 2 and name[0].isalpha() and name[1] == ":")
    )


def _contained(parts: tuple[str, ...]) -> bool:
    depth = 0
    for part in parts:
        if part in ("", "."):
            continue
        if part == "..":
            depth -= 1
            if depth < 0:
                return False
        else:
            depth += 1
    return True


def _member_path(name: str) -> PurePosixPath:
    if not name or "\x00" in name or "\\" in name or _is_absolute(name):
        raise UnsafeArchive(f"unsafe archive member path: {name!r}")
    path = PurePosixPath(name)
    if ".." in path.parts:
        raise UnsafeArchive(f"unsafe archive member path: {name!r}")
    return path


def _stripped_path(path: PurePosixPath, components: int) -> PurePosixPath | None:
    if len(path.parts) <= components:
        return None
    return PurePosixPath(*path.parts[components:])


def _validate_link(
    member: PurePosixPath, target: str, *, hardlink: bool, strip_components: int
) -> None:
    if not target or "\x00" in target or "\\" in target or _is_absolute(target):
        raise UnsafeArchive(
            f"unsafe archive link target: {member.as_posix()!r} -> {target!r}"
        )
    target_path = PurePosixPath(target)
    if hardlink:
        target_path = _member_path(target)
        stripped_target = _stripped_path(target_path, strip_components)
        if stripped_target is None:
            raise UnsafeArchive(
                f"unsafe archive link target removed by stripping: "
                f"{member.as_posix()!r} -> {target!r}"
            )
        rooted = stripped_target.parts
    else:
        rooted = member.parent.parts + target_path.parts
    if not _contained(rooted):
        raise UnsafeArchive(
            f"unsafe archive link target: {member.as_posix()!r} -> {target!r}"
        )


def _validate_tar(path: Path, strip_components: int) -> None:
    with tarfile.open(path, "r:*") as archive:
        for item in archive:
            archived_member = _member_path(item.name)
            member = _stripped_path(archived_member, strip_components)
            if member is None:
                continue
            if item.issym():
                _validate_link(
                    member,
                    item.linkname,
                    hardlink=False,
                    strip_components=strip_components,
                )
            elif item.islnk():
                _validate_link(
                    member,
                    item.linkname,
                    hardlink=True,
                    strip_components=strip_components,
                )
            elif item.isdev() or item.isfifo():
                raise UnsafeArchive(f"unsafe archive special member: {item.name!r}")


def _validate_zip(path: Path) -> None:
    with zipfile.ZipFile(path) as archive:
        for item in archive.infolist():
            member = _member_path(item.filename)
            mode = item.external_attr >> 16
            if stat.S_ISLNK(mode):
                try:
                    target = archive.read(item).decode("utf-8")
                except UnicodeDecodeError as error:
                    raise UnsafeArchive(
                        f"unsafe archive link target encoding: {item.filename!r}"
                    ) from error
                _validate_link(
                    member, target, hardlink=False, strip_components=0
                )


def validate_archive(path: Path, strip_components: int = 0) -> None:
    if tarfile.is_tarfile(path):
        _validate_tar(path, strip_components)
        return
    if zipfile.is_zipfile(path):
        if strip_components:
            raise UnsafeArchive("unsafe archive: zip extraction cannot strip components")
        _validate_zip(path)
        return
    raise UnsafeArchive(f"unsafe archive: unsupported format: {path}")


def canonical_executable(root: Path, candidate: Path) -> Path:
    verified_root = root.resolve(strict=True)
    executable = candidate.resolve(strict=True)
    try:
        executable.relative_to(verified_root)
    except ValueError as error:
        raise ValueError(
            f"backend path resolves outside required root: {candidate} -> {executable}"
        ) from error
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError(f"backend path is not an executable regular file: {executable}")
    return executable


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    archive_parser = subparsers.add_parser("archive")
    archive_parser.add_argument("--strip-components", type=int, default=0)
    archive_parser.add_argument("archive", type=Path)
    path_parser = subparsers.add_parser("path")
    path_parser.add_argument("root", type=Path)
    path_parser.add_argument("candidate", type=Path)
    args = parser.parse_args(argv)

    try:
        if args.command == "archive":
            if args.strip_components < 0:
                raise ValueError("strip-components must be non-negative")
            validate_archive(args.archive, args.strip_components)
        else:
            print(canonical_executable(args.root, args.candidate))
    except (OSError, tarfile.TarError, zipfile.BadZipFile, ValueError) as error:
        print(f"simulator provenance: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
