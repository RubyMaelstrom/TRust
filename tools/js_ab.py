#!/usr/bin/env python3
"""Interleaved A/B timing of JavaScript benchmark scripts across engine binaries.

    tools/js_ab.py --base BIN --cand BIN [--node] [-n ROUNDS] [--cpu N] SCRIPT.js [...]

Each round runs every engine once per script, alternating the engine order between rounds so
frequency and thermal drift affect both sides equally, pinned with `taskset -c N`. Every stdout
line of the form `name: <number> ...` is a result; the report shows the median per engine and
the candidate/baseline ratio (below 1 is faster for times, above 1 is better for scores).
`--node` adds a Node.js column for reference. A non-zero exit is reported with its stderr tail.
Scripts should print with `typeof print === "function" ? print : console.log` so the same file
runs everywhere.
"""
import argparse
import collections
import re
import statistics
import subprocess

LINE = re.compile(r"^([^:]+?):\s*([0-9.]+)")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", required=True, help="baseline engine binary")
    parser.add_argument("--cand", required=True, help="candidate engine binary")
    parser.add_argument("--node", action="store_true", help="add a Node.js column")
    parser.add_argument("-n", "--rounds", type=int, default=3, help="rounds (default 3)")
    parser.add_argument("--cpu", default="5", help="CPU list for taskset (default 5)")
    parser.add_argument("--timeout", type=int, default=1800, help="seconds per run")
    parser.add_argument("scripts", nargs="+")
    args = parser.parse_args()
    engines = [("base", args.base), ("cand", args.cand)] + ([("node", "node")] if args.node else [])
    for script in args.scripts:
        results = collections.defaultdict(lambda: collections.defaultdict(list))
        for r in range(args.rounds):
            for name, binary in engines if r % 2 == 0 else list(reversed(engines)):
                out = subprocess.run(["taskset", "-c", args.cpu, binary, script],
                                     capture_output=True, text=True, timeout=args.timeout)
                for line in out.stdout.splitlines():
                    m = LINE.match(line.strip())
                    if m:
                        results[m.group(1)][name].append(float(m.group(2)))
                if out.returncode != 0:
                    print(f"!! {name} exit {out.returncode}: {out.stderr[-300:]}")
        print(f"== {script}")
        header = f"{'benchmark':32s} {'base':>10s} {'cand':>10s} {'cand/base':>9s}"
        print(header + (f" {'node':>10s}" if args.node else ""))
        for bench, per in results.items():
            b = statistics.median(per["base"]) if per["base"] else float("nan")
            c = statistics.median(per["cand"]) if per["cand"] else float("nan")
            row = f"{bench:32s} {b:10.1f} {c:10.1f} {c / b if b else float('nan'):9.3f}"
            if args.node:
                v = statistics.median(per["node"]) if per["node"] else float("nan")
                row += f" {v:10.1f}"
            print(row)


if __name__ == "__main__":
    main()
