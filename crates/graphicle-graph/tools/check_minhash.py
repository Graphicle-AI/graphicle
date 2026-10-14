#!/usr/bin/env python3
"""Compare sketch bytes, coefficient families, band choices and candidate sets."""
import functools
import random
import numpy as np
from parity import Native, upstream_tests
import graphify._minhash as reference

native = Native("minhash")
OriginalMinHash = reference.MinHash
OriginalLsh = reference.MinHashLSH
original_params = reference._optimal_lsh_params
rng = random.Random(20260923)


class NativeMinHash:
    def __init__(self, num_perm=128):
        self.num_perm = num_perm
        self._chunks = []
        self._refresh()

    def _refresh(self):
        result = native.result("minhash", num_perm=self.num_perm, chunks=self._chunks)
        self.hashvalues = np.array(result["hashvalues"], dtype=np.uint64)
        self._a = np.array(result["a"], dtype=np.uint64)
        self._b = np.array(result["b"], dtype=np.uint64)

    def update(self, value):
        self._chunks.append(list(value))
        self._refresh()


@functools.cache
def native_params(threshold, num_perm):
    return tuple(native.result("minhash.params", threshold=threshold, num_perm=num_perm))


class NativeLsh:
    def __init__(self, threshold=0.5, num_perm=128):
        self.threshold, self.num_perm = threshold, num_perm
        self.b, self.r = native_params(threshold, num_perm)
        self._entries = []

    def _call(self, entries, queries):
        return native.result("minhash.lsh", threshold=self.threshold, num_perm=self.num_perm,
                             entries=entries, queries=queries)

    def insert(self, key, minhash):
        entry = {"key": key, "hashvalues": minhash.hashvalues.tolist()}
        result = self._call([*self._entries, entry], [])
        if result["errors"]:
            raise ValueError(result["errors"][-1])
        self._entries.append(entry)

    def query(self, minhash):
        return self._call(self._entries, [minhash.hashvalues.tolist()])["results"][0]


try:
    sketches_checked = 0
    for n in [0, 1, 2, 3, 4, 7, 16, 32, 64, 127, 128, 129, 256, 512, 1024]:
        chunks = [b"", b"hello", bytes(range(256)), "é日本語".encode()]
        chunks += [rng.randbytes(rng.randrange(100)) for _ in range(30)]
        oracle = OriginalMinHash(n)
        native_sketch = NativeMinHash(n)
        for upto in range(len(chunks) + 1):
            if upto:
                oracle.update(chunks[upto - 1])
                native_sketch.update(chunks[upto - 1])
            for field in ("hashvalues", "_a", "_b"):
                expected = getattr(oracle, field).tolist()
                actual = getattr(native_sketch, field).tolist()
                if expected != actual:
                    native.fail(num_perm=n, chunks=[list(v) for v in chunks[:upto]], field=field, expected=expected, actual=actual)
            sketches_checked += 1

    parameters_checked = 0
    for n in [0, 1, 2, 3, 8, 32, 64, 128, 256]:
        for threshold in [0.0, 0.01, 0.25, 0.5, 0.65, 0.8, 0.95, 0.99, 1.0]:
            expected = original_params(threshold, n)
            actual = native_params(threshold, n)
            if expected != actual:
                native.fail(threshold=threshold, num_perm=n, expected=expected, actual=actual)
            parameters_checked += 1

    candidate_queries_checked = 0
    for n in [0, 1, 8, 32, 128]:
        sketches = []
        common = [rng.randbytes(3) for _ in range(20)]
        for i in range(30):
            sketch = OriginalMinHash(n)
            for chunk in common[:i % 21] + [rng.randbytes(3) for _ in range(5)]:
                sketch.update(chunk)
            sketches.append(sketch)
        for threshold in [0.01, 0.5, 0.95, 1.0]:
            oracle = OriginalLsh(threshold=threshold, num_perm=n)
            entries = []
            errors = []
            for i, sketch in enumerate([*sketches, sketches[0]]):
                key = "quote'\"\\\n" if i % 30 == 0 else str(i)
                entries.append({"key": key, "hashvalues": sketch.hashvalues.tolist()})
                try:
                    oracle.insert(key, sketch)
                except ValueError as error:
                    errors.append(str(error))
            expected = {"b": oracle.b, "r": oracle.r, "errors": errors,
                        "results": [sorted(oracle.query(sketch)) for sketch in sketches]}
            actual = native.result("minhash.lsh", threshold=threshold, num_perm=n,
                                   entries=entries, queries=[sketch.hashvalues.tolist() for sketch in sketches])
            if expected != actual:
                native.fail(threshold=threshold, num_perm=n, expected=expected, actual=actual)
            candidate_queries_checked += len(sketches)

    reference.MinHash = NativeMinHash
    reference.MinHashLSH = NativeLsh
    reference._optimal_lsh_params = native_params
    tests = upstream_tests("tests/test_minhash.py")
finally:
    native.close()

native.receipt("graphify/_minhash.py", ["src/minhash.rs"],
               sketch_states_and_coefficient_pairs=sketches_checked,
               optimal_parameters_checked=parameters_checked,
               candidate_queries_checked=candidate_queries_checked,
               upstream_tests=tests,
               ordering="Query result sets match; Rust returns a stable sorted list instead of Python set order.",
               test_scope="MinHash, LSH and parameter selection are native; Python/Numpy only provide the test oracle and array interface.")
