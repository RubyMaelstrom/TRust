#!/usr/bin/env python3
"""Attribute JIT samples to TRust JS template families.

    tools/jit_families.py OUT.txt [--freq HZ] -- COMMAND [ARGS...]

A thin wrapper over tools/psample.py (same options and requirements: Linux, perf_event_paranoid
of 2 or lower, ARM64 TRust JS JIT). The command runs with TRUST_JS_JIT_MAP=1, whose stderr
(`OUT.txt.jitmap`) now carries `[jit-map-mark] <base> <offset> <family>` lines beside the
per-operation `[jit-map-pc]` lines: each marks where the code of one template family begins
(`typed-load`, `typed-store`, `get-elem`, `set-elem`, `load-name`, `call`, `chain`,
`helper-call`; `-` closes a nested family back to its parent). Every sample in generated code
is charged to the family of the nearest preceding mark, else to the operation whose region
contains it ("other"). Samples outside generated code are "rust/other".

The report is printed to stdout: the share of each family and, for "other", the operations whose
regions hold the most samples. Region-compiled functions put their whole body under one
operation, so "other" is the cost of the register-region lowering, not of that one operation.
"""
import bisect
import collections
import importlib.util
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("psample", os.path.join(HERE, "psample.py"))
ps = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ps)


def load_marks(path):
    marks = collections.defaultdict(list)
    for line in open(path, errors="replace"):
        if line.startswith("[jit-map-mark] "):
            _, base, offset, name = line.rstrip("\n").split(" ", 3)
            marks[int(base, 16)].append((int(offset, 16), name))
    for entries in marks.values():
        entries.sort(key=lambda entry: entry[0])
    return marks


def report(args, samples, threads, sym):
    marks = load_marks(args.out + ".jitmap")
    if args.thread:
        samples = [s for s in samples if args.thread in threads.get(s[1], str(s[1]))]
    total = len(samples)
    families, ops = collections.Counter(), collections.Counter()
    jit = 0
    for ip, _tid, _chain in samples:
        found = sym.jit_range(ip)
        if found is None:
            families["rust/other"] += 1
            continue
        jit += 1
        base, _length, _name, pcs = found
        entries = marks.get(base, [])
        k = bisect.bisect_right([e[0] for e in entries], ip - base) - 1
        family = entries[k][1] if k >= 0 else "-"
        if family != "-":
            families["jit:" + family] += 1
            continue
        j = bisect.bisect_right([p[0] for p in pcs], ip - base) - 1
        ops[pcs[j][2].split("(")[0] if j >= 0 else "prologue"] += 1
        families["jit:other"] += 1
    print(f"samples {total}: generated code {100 * jit / max(total, 1):.1f}%")
    for name, count in families.most_common():
        print(f"{100 * count / max(total, 1):6.2f}% {name}")
    print("-- 'other' by the operation whose region holds the sample --")
    for name, count in ops.most_common(12):
        print(f"{100 * count / max(total, 1):6.2f}% {name}")


if __name__ == "__main__":
    if "--jitmap" not in sys.argv:
        sys.argv.insert(sys.argv.index("--"), "--jitmap")
    ps.report = report
    ps.main()
