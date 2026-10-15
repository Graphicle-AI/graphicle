#!/usr/bin/env python3
"""Tests of release_build.py's Windows packaging: program names, the PE import
check and the reproducible .zip. They build small PE files by hand, so they run
on any system: python3 packaging/test_release_build.py"""
import struct
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_build  # noqa: E402

WINDOWS = 'x86_64-pc-windows-msvc'


def pe_file(imports, delayed=(), plus=True, relative_delay=True):
    """A minimal PE program with one section that holds its import tables and
    library names. delayed: delay-loaded libraries, named by relative or (old
    style) virtual addresses."""
    image_base, section_rva, section_raw = 0x140000000 if plus else 0x400000, 0x1000, 0x400
    optional_size = (112 if plus else 96) + 16 * 8
    names, blob = {}, b''
    tables = 20 * (len(imports) + 1) + 32 * (len(delayed) + 1)
    for library in [*imports, *delayed]:
        names[library] = section_rva + tables + len(blob)
        blob += library.encode('ascii') + b'\0'
    data = b''.join(struct.pack('<5I', 0, 0, 0, names[library], 0) for library in imports) + bytes(20)
    import_rva, delay_rva = section_rva, section_rva + 20 * (len(imports) + 1)
    data += b''.join(struct.pack('<8I', 1 if relative_delay else 0,
                                 names[library] if relative_delay else names[library] + image_base, 0, 0, 0, 0, 0, 0)
                     for library in delayed) + bytes(32)
    data += blob
    optional = bytearray(optional_size)
    struct.pack_into('<H', optional, 0, 0x20B if plus else 0x10B)
    if plus:
        struct.pack_into('<Q', optional, 24, image_base)
    else:
        struct.pack_into('<I', optional, 28, image_base)
    directories = 112 if plus else 96
    struct.pack_into('<I', optional, directories - 4, 16)
    struct.pack_into('<II', optional, directories + 8, import_rva, 20 * (len(imports) + 1))
    if delayed:
        struct.pack_into('<II', optional, directories + 8 * 13, delay_rva, 32 * (len(delayed) + 1))
    header = bytearray(0x40)
    header[:2] = b'MZ'
    struct.pack_into('<I', header, 0x3C, 0x40)
    coff = struct.pack('<HHIIIHH', 0x8664 if plus else 0x14C, 1, 0, 0, 0, optional_size, 0x22)
    section = struct.pack('<8s4I12xI', b'.idata', len(data), section_rva, len(data), section_raw, 0xC0000040)
    image = bytes(header) + b'PE\0\0' + coff + bytes(optional) + section
    return image + bytes(section_raw - len(image)) + data


class ProgramNames(unittest.TestCase):
    def test_exe_on_windows_only(self):
        self.assertEqual(release_build.program_file('graphicle', WINDOWS), 'graphicle.exe')
        self.assertEqual(release_build.program_file('graphicle-graph', WINDOWS), 'graphicle-graph.exe')
        for target in ('x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'):
            self.assertEqual(release_build.program_file('graphicle', target), 'graphicle')


class PeImports(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)

    def write(self, data, name='graphicle.exe'):
        path = Path(self.folder.name) / name
        path.write_bytes(data)
        return path

    def test_reads_imports_and_delay_loads(self):
        imports = ['KERNEL32.dll', 'ntdll.dll', 'api-ms-win-core-synch-l1-2-0.dll', 'bcryptprimitives.dll']
        path = self.write(pe_file(imports, delayed=['SHELL32.dll']))
        self.assertEqual(release_build.pe_imports(path), imports + ['SHELL32.dll'])

    def test_pe32_and_old_style_delay_loads(self):
        path = self.write(pe_file(['KERNEL32.dll'], delayed=['ole32.dll'], plus=False, relative_delay=False))
        self.assertEqual(release_build.pe_imports(path), ['KERNEL32.dll', 'ole32.dll'])

    def test_real_programs(self):
        """The Windows launchers pip ships (x86, x64 and ARM64 builds), when
        this Python has pip."""
        try:
            import pip
        except ImportError:
            self.skipTest('pip is not installed')
        samples = sorted((Path(pip.__file__).parent / '_vendor' / 'distlib').glob('*.exe'))
        if not samples:
            self.skipTest("pip's Windows launchers are not installed")
        for sample in samples:
            imports = release_build.pe_imports(sample)
            self.assertIn('kernel32.dll', [name.lower() for name in imports], sample.name)

    def test_not_a_pe_file(self):
        with self.assertRaises(ValueError):
            release_build.pe_imports(self.write(b'\x7fELF' + bytes(100)))
        with self.assertRaises(ValueError):
            release_build.pe_imports(self.write(b'MZ'))

    def test_system_libraries_pass(self):
        allowed = ['KERNEL32.dll', 'ntdll.dll', 'ADVAPI32.dll', 'WS2_32.dll', 'bcrypt.dll', 'bcryptprimitives.dll',
                   'USERENV.dll', 'ole32.dll', 'OLEAUT32.dll', 'SHELL32.dll', 'CRYPT32.dll', 'Secur32.dll',
                   'ncrypt.dll', 'api-ms-win-core-synch-l1-2-0.dll']
        self.assertEqual(release_build.windows_foreign(allowed), ([], []))
        path = self.write(pe_file(allowed))
        self.assertEqual(release_build.check_linkage(path, WINDOWS), (allowed, None))

    def test_c_runtime_and_other_libraries_fail(self):
        runtime = ['VCRUNTIME140.dll', 'MSVCP140.dll', 'api-ms-win-crt-runtime-l1-1-0.dll']
        self.assertEqual(release_build.windows_foreign(['KERNEL32.dll', *runtime, 'USER32.dll']),
                         (runtime, ['USER32.dll']))
        for libraries in (['KERNEL32.dll', 'VCRUNTIME140.dll'], ['KERNEL32.dll', 'libssl-3-x64.dll']):
            with self.assertRaises(SystemExit):
                release_build.check_linkage(self.write(pe_file(libraries)), WINDOWS)
        with self.assertRaises(SystemExit):
            release_build.check_linkage(self.write(pe_file(['KERNEL32.dll'], delayed=['VCRUNTIME140_1.dll'])),
                                        WINDOWS)


class Archives(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name)
        sources = self.root / 'sources'
        (sources / 'licenses').mkdir(parents=True)
        (sources / 'graphicle.exe').write_bytes(b'program' * 1000)
        (sources / 'LICENSE').write_text('terms\n')
        (sources / 'licenses' / 'a.txt').write_text('a\n')
        name = 'graphicle-0.1.0-' + WINDOWS
        self.members = sorted([(f'{name}/graphicle.exe', sources / 'graphicle.exe', 0o755),
                               (f'{name}/LICENSE', sources / 'LICENSE', 0o644),
                               (f'{name}/licenses/a.txt', sources / 'licenses' / 'a.txt', 0o644)])
        self.name = name

    def test_zip_is_reproducible(self):
        first, second = self.root / 'first.zip', self.root / 'second.zip'
        release_build.write_archive(first, self.members, 1760400000)
        release_build.write_archive(second, self.members, 1760400000)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with zipfile.ZipFile(first) as bundle:
            entries = bundle.infolist()
            self.assertEqual([entry.filename for entry in entries],
                             [f'{self.name}/', f'{self.name}/licenses/', f'{self.name}/LICENSE',
                              f'{self.name}/graphicle.exe', f'{self.name}/licenses/a.txt'])
            self.assertEqual({entry.date_time for entry in entries}, {(2025, 10, 14, 0, 0, 0)})
            self.assertEqual(bundle.read(f'{self.name}/graphicle.exe'), b'program' * 1000)
            modes = {entry.filename: entry.external_attr >> 16 for entry in entries}
            self.assertEqual(modes[f'{self.name}/graphicle.exe'], 0o100755)
            self.assertEqual(modes[f'{self.name}/LICENSE'], 0o100644)
            self.assertTrue(bundle.getinfo(f'{self.name}/').is_dir())
            self.assertIsNone(bundle.testzip())

    def test_zip_times_start_in_1980(self):
        archive = self.root / 'old.zip'
        release_build.write_archive(archive, self.members, 0)
        with zipfile.ZipFile(archive) as bundle:
            self.assertEqual({entry.date_time for entry in bundle.infolist()}, {(1980, 1, 1, 0, 0, 0)})

    def test_tarball_unchanged(self):
        archive = self.root / 'unix.tar.gz'
        release_build.write_archive(archive, self.members, 1760400000)
        with tarfile.open(archive) as tar:
            self.assertEqual(tar.getnames()[:2], [f'{self.name}', f'{self.name}/licenses'])
            self.assertEqual(tar.getmember(f'{self.name}/graphicle.exe').mode, 0o755)
        self.assertEqual(archive.read_bytes()[:2], b'\x1f\x8b')


if __name__ == '__main__':
    unittest.main()
