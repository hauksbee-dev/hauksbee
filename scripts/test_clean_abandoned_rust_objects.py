#!/usr/bin/env python3
"""Safety checks for the opt-in local Rust object cleanup."""
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import clean_abandoned_rust_objects as cleaner


@unittest.skipUnless(os.name == 'posix', 'local Unix build cleanup')
class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.profile = self.root / 'target/.build/debug'
        self.deps = self.profile / 'deps'
        self.deps.mkdir(parents=True)
        (self.root / 'target/debug').mkdir()
        self.lock = self.profile / '.cargo-lock'
        self.lock.touch()
        (self.root / 'target/debug/.cargo-lock').touch()
        self.object = self.deps / 'crate.hash.rcgu.o'
        self.object.write_bytes(b'object')
        os.utime(self.object, (1, 1))

    def run_clean(self, apply=True):
        with patch.object(cleaner, 'require_idle'):
            return cleaner.clean(self.root, apply)

    def test_removes_only_regular_rcgu_objects_and_preserves_cache(self):
        keep = ['libcrate.rlib', 'libcrate.rmeta', 'crate.d', 'other.o', 'program']
        for name in keep:
            (self.deps / name).write_bytes(b'keep')
        external = self.root / 'outside.rcgu.o'
        external.write_bytes(b'external')
        (self.deps / 'symlink.rcgu.o').symlink_to(external)
        (self.deps / 'directory.rcgu.o').mkdir()
        cache = self.profile / 'incremental'
        cache.mkdir()
        retained = cache / 'kept.o'
        os.link(self.object, retained)
        result = self.run_clean()
        self.assertEqual(result['removed'], 1)
        self.assertFalse(self.object.exists())
        self.assertEqual(retained.read_bytes(), b'object')
        self.assertEqual(external.read_bytes(), b'external')
        self.assertTrue((self.deps / 'symlink.rcgu.o').is_symlink())
        self.assertTrue((self.deps / 'directory.rcgu.o').is_dir())
        for name in keep:
            self.assertEqual((self.deps / name).read_bytes(), b'keep')
        self.assertEqual(len(list((self.root / 'target/build-cleanup').glob('*.json'))), 1)

    def test_dry_run_and_recent_files_are_preserved(self):
        result = self.run_clean(False)
        self.assertEqual(result['matched'], 1)
        self.assertEqual(result['removed'], 0)
        self.assertTrue(self.object.exists())
        os.utime(self.object, (4102444800, 4102444800))
        self.assertEqual(self.run_clean()['recent_entries'], 1)
        self.assertTrue(self.object.exists())

    def test_active_compiler_refuses_before_deletion(self):
        with patch.object(cleaner, 'require_idle', side_effect=cleaner.Busy('rustc')):
            with self.assertRaises(cleaner.Busy):
                cleaner.clean(self.root, True)
        self.assertTrue(self.object.exists())

    def test_cargo_lock_blocks_cleanup(self):
        command = [sys.executable, '-c',
                   'import fcntl,sys; f=open(sys.argv[1],"r+"); '
                   'fcntl.flock(f,fcntl.LOCK_EX); print("locked",flush=True); sys.stdin.read()',
                   str(self.lock)]
        with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
            self.assertEqual(child.stdout.readline().strip(), 'locked')
            try:
                with self.assertRaises(cleaner.Busy):
                    self.run_clean()
                self.assertTrue(self.object.exists())
            finally:
                child.communicate(timeout=5)

    def test_redirected_directory_or_lock_is_rejected(self):
        destination = self.deps.with_name('elsewhere')
        self.deps.rename(destination)
        self.deps.symlink_to(destination, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, 'redirected'):
            self.run_clean()
        self.deps.unlink()
        destination.rename(self.deps)
        self.lock.unlink()
        self.lock.symlink_to(self.object)
        with self.assertRaisesRegex(RuntimeError, 'Cargo lock'):
            self.run_clean()
        self.assertTrue(self.object.exists())

    def test_wrapper_preserves_exit_status_arguments_and_explicit_overrides(self):
        scripts = self.root / 'scripts'
        scripts.mkdir()
        wrapper = Path(__file__).with_name('dev-cargo.sh')
        shutil.copy2(wrapper, scripts / wrapper.name)
        (scripts / 'clean_abandoned_rust_objects.py').write_text('print("cleanup called")')
        cargo = self.root / 'cargo'
        cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\nexit 7\n')
        cargo.chmod(0o700)
        for args, calls in [(['check', 'argument with spaces'], 2),
                            (['check', '--target-dir', '/elsewhere'], 0),
                            (['rustc', '--', '-Csave-temps'], 0)]:
            with self.subTest(args=args):
                result = subprocess.run(
                    ['bash', str(scripts / wrapper.name), *args], capture_output=True, text=True,
                    env={**os.environ, 'PATH': str(self.root) + os.pathsep + os.environ['PATH']},
                )
                self.assertEqual(result.returncode, 7)
                self.assertEqual(result.stdout.count('cleanup called'), calls)
                self.assertIn('\n'.join(args) + '\n', result.stdout)

    def test_process_check_recognizes_compiler_paths(self):
        for process in ['rustc', '/usr/bin/ld', '/toolchain/bin/cargo']:
            with self.subTest(process=process), patch.object(
                cleaner.subprocess, 'check_output', return_value=process + '\n'
            ):
                with self.assertRaises(cleaner.Busy):
                    cleaner.require_idle()


if __name__ == '__main__':
    unittest.main()
