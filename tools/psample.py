#!/usr/bin/env python3
"""User-space CPU sampler for TRust and Lumen, built directly on perf_event_open.

    tools/psample.py OUT.txt [--freq HZ] [--depth N] [--jitmap] -- COMMAND [ARGS...]

Starts COMMAND stopped, attaches a task-clock sampling event (user space only, so the default
`kernel.perf_event_paranoid` of 2 suffices), resumes it and records the instruction pointer and
frame-pointer call chain of every sample until the process exits. Samples are symbolized against
the ELF symbol tables of the mapped executables (`nm`, `readelf`; names demangled with
`rustfilt` or `c++filt` when available) and written to OUT.txt:

  * self and inclusive percentages per function;
  * the leading callers of the hottest functions and the most frequent 5-frame stacks;
  * an approximate category rollup (JIT code, JIT helpers, calls, property access, ...);
  * with PSAMPLE_HOT=<substring>: a per-instruction histogram for matching functions
    (addresses are executable virtual addresses for `objdump --start-address`);
  * with --jitmap and PSAMPLE_JITHOT=<substring>: a per-offset histogram inside matching
    Lumen JIT functions (offsets line up with a LUMEN_JIT_CODEDUMP of the same function).

Anonymous executable mappings are reported as `[jit]`. With --jitmap the child runs with
LUMEN_JIT_MAP=1, its stderr goes to OUT.txt.jitmap, and JIT samples are attributed to
`[jit <first local names>] pc<N> <bytecode op>` (the region between two op offsets is charged to
the earlier op, so fused templates and shared stubs appear under a neighbouring op).

Build the profiled binary with frame pointers in a separate target directory, e.g.
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --target-dir target/fp
otherwise only self times are reliable. Pin the command (for example with `taskset -c N`)
for repeatable attribution. Linux only.
"""
import argparse
import bisect
import collections
import ctypes
import mmap
import os
import re
import signal
import struct
import subprocess
import sys
import time

PERF_TYPE_SOFTWARE, PERF_COUNT_SW_TASK_CLOCK = 1, 1
SAMPLE_IP, SAMPLE_TID, SAMPLE_CALLCHAIN = 1, 2, 32
PERF_RECORD_SAMPLE = 9
PERF_CONTEXT_MAX = (1 << 64) - 4095  # callchain context markers are at or above this value
NR_PERF_EVENT_OPEN = {"aarch64": 241, "x86_64": 298}


def parse_args(argv):
    if "--" not in argv:
        sys.exit("usage: psample.py OUT.txt [--freq HZ] [--depth N] [--jitmap] -- COMMAND ...")
    split = argv.index("--")
    parser = argparse.ArgumentParser(prog="psample.py")
    parser.add_argument("out", help="report path")
    parser.add_argument("--freq", type=int, default=4000, help="samples per second (default 4000)")
    parser.add_argument("--depth", type=int, default=24, help="call-chain frames kept (default 24)")
    parser.add_argument("--jitmap", action="store_true", help="attribute Lumen JIT code by op")
    parser.add_argument("--thread", help="report only samples from threads whose name contains this")
    args = parser.parse_args(argv[:split])
    args.command = argv[split + 1:]
    if not args.command:
        parser.error("missing COMMAND after --")
    return args


def open_event(pid, cpu, freq, depth):
    syscall = NR_PERF_EVENT_OPEN.get(os.uname().machine)
    if syscall is None:
        raise SystemExit(f"unsupported machine {os.uname().machine}")
    attr = bytearray(128)
    # disabled | inherit | exclude_kernel | exclude_hv | freq | enable_on_exec |
    # exclude_callchain_kernel. Threads the command spawns inherit the event; the kernel only
    # allows mapping an inherited task event per CPU, so `run` opens one per CPU.
    flags = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 6) | (1 << 10) | (1 << 12) | (1 << 21)
    struct.pack_into("<IIQQQQQ", attr, 0, PERF_TYPE_SOFTWARE, 128, PERF_COUNT_SW_TASK_CLOCK,
                     freq, SAMPLE_IP | SAMPLE_TID | SAMPLE_CALLCHAIN, 0, flags)
    struct.pack_into("<H", attr, 108, depth + 2)  # sample_max_stack
    libc = ctypes.CDLL(None, use_errno=True)
    buf = ctypes.create_string_buffer(bytes(attr), 128)
    fd = libc.syscall(syscall, buf, pid, cpu, -1, 0)
    if fd < 0:
        raise OSError(ctypes.get_errno(), "perf_event_open failed (check kernel.perf_event_paranoid)")
    return fd


class Ring:
    # One ring per CPU: 64 data pages each keeps all of them inside the default
    # kernel.perf_event_mlock_kb allowance; the loop drains them every few milliseconds.
    PAGES = 1 + 64

    def __init__(self, fd):
        self.page = mmap.PAGESIZE
        self.data_size = (self.PAGES - 1) * self.page
        self.map = mmap.mmap(fd, self.PAGES * self.page, mmap.MAP_SHARED,
                             mmap.PROT_READ | mmap.PROT_WRITE)

    def drain(self, samples):
        head = struct.unpack_from("<Q", self.map, 1024)[0]
        tail = struct.unpack_from("<Q", self.map, 1032)[0]
        data = self.map[self.page:]
        while tail < head:
            off = tail % self.data_size
            header = self._read(data, off, 8)
            kind, _misc, size = struct.unpack("<IHH", header)
            if kind == PERF_RECORD_SAMPLE:
                record = self._read(data, off, size)
                ip, _pid, tid, count = struct.unpack_from("<QIIQ", record, 8)
                samples.append((ip, tid, struct.unpack_from(f"<{count}Q", record, 32)))
            tail += size
        struct.pack_into("<Q", self.map, 1032, tail)

    def _read(self, data, off, size):
        if off + size <= self.data_size:
            return bytes(data[off:off + size])
        return bytes(data[off:]) + bytes(data[:off + size - self.data_size])


def run(args):
    jitmap_path = args.out + ".jitmap"
    pid = os.fork()
    if pid == 0:
        if args.jitmap:
            os.environ["LUMEN_JIT_MAP"] = "1"
            err = os.open(jitmap_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
            os.dup2(err, 2)
        os.kill(os.getpid(), signal.SIGSTOP)
        os.execvp(args.command[0], args.command)
    os.waitpid(pid, os.WUNTRACED)
    rings = []
    try:
        for cpu in sorted(os.sched_getaffinity(0)):
            rings.append(Ring(open_event(pid, cpu, args.freq, args.depth)))
    except OSError:
        os.kill(pid, signal.SIGKILL)
        raise
    os.kill(pid, signal.SIGCONT)
    samples, maps_text, exe, last = [], "", None, 0.0
    threads = {}
    while True:
        done, _status = os.waitpid(pid, os.WNOHANG)
        now = time.time()
        if not done and now - last > 0.5:
            # The last snapshot before exit describes the final executable and JIT mappings.
            try:
                with open(f"/proc/{pid}/maps") as f:
                    maps_text = f.read() or maps_text
                exe = os.readlink(f"/proc/{pid}/exe")
                for tid in os.listdir(f"/proc/{pid}/task"):
                    with open(f"/proc/{pid}/task/{tid}/comm") as f:
                        threads[int(tid)] = f.read().strip()
            except OSError:
                pass
            last = now
        for ring in rings:
            ring.drain(samples)
        if done:
            break
        time.sleep(0.005)
    for ring in rings:
        ring.drain(samples)
    return samples, threads, maps_text, exe, jitmap_path


class Symbolizer:
    def __init__(self, maps_text, exe, jitmap_path):
        self.exe = exe
        self.regions = []
        for line in maps_text.splitlines():
            parts = line.split()
            if "x" not in parts[1]:
                continue
            start, end = (int(x, 16) for x in parts[0].split("-"))
            path = parts[5] if len(parts) > 5 else ""
            self.regions.append((start, end, path, int(parts[2], 16)))
        self.regions.sort()
        self.starts = [r[0] for r in self.regions]
        self.symbols = {}
        self.cache = {}
        self.jit = self._load_jitmap(jitmap_path)
        self.jit_starts = [r[0] for r in self.jit]

    @staticmethod
    def _load_jitmap(path):
        if not path or not os.path.exists(path):
            return []
        ranges = {}
        for line in open(path, errors="replace"):
            if line.startswith("[jit-map-range] "):
                _, base, length, *name = line.rstrip("\n").split(" ", 3)
                ranges[int(base, 16)] = [int(base, 16), int(length, 16), name[0] if name else "?", []]
            elif line.startswith("[jit-map-pc] "):
                parts = line.rstrip("\n").split(" ", 5)
                base, off = int(parts[1], 16), int(parts[2], 16)
                if base in ranges:
                    ranges[base][3].append((off, parts[3], parts[4] if len(parts) > 4 else "?"))
        for r in ranges.values():
            r[3].sort()
        return sorted(ranges.values())

    def jit_range(self, addr):
        i = bisect.bisect_right(self.jit_starts, addr) - 1
        if i >= 0 and addr < self.jit_starts[i] + self.jit[i][1]:
            return self.jit[i]
        return None

    def _jit_name(self, addr):
        r = self.jit_range(addr)
        if r is None:
            return "[jit]"
        base, _length, name, pcs = r
        j = bisect.bisect_right([p[0] for p in pcs], addr - base) - 1
        where = f" pc{pcs[j][1]} {pcs[j][2][:40]}" if j >= 0 else " prologue"
        return f"[jit {name[:40]}]{where}"

    def _load(self, path):
        if path not in self.symbols:
            loads, addrs, names = [], [], []
            try:
                ph = subprocess.run(["readelf", "-lW", path], capture_output=True, text=True).stdout
                for m in re.finditer(r"LOAD\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x[0-9a-f]+\s+0x([0-9a-f]+)", ph):
                    loads.append((int(m.group(1), 16), int(m.group(2), 16), int(m.group(3), 16)))
                nm = subprocess.run(["nm", "-n", "--defined-only", path], capture_output=True, text=True).stdout
                for line in nm.splitlines():
                    p = line.split(None, 2)
                    if len(p) == 3 and p[1] in "tTwW":
                        addrs.append(int(p[0], 16))
                        names.append(p[2])
            except FileNotFoundError:
                pass
            self.symbols[path] = (loads, addrs, names)
        return self.symbols[path]

    def vaddr(self, addr):
        """(path, executable virtual address) of a file-backed address, else None."""
        i = bisect.bisect_right(self.starts, addr) - 1
        if i < 0 or addr >= self.regions[i][1] or not self.regions[i][2].startswith("/"):
            return None
        start, _end, path, offset = self.regions[i]
        loads, _, _ = self._load(path)
        file_off = addr - start + offset
        for poff, pvaddr, filesz in loads:
            if poff <= file_off < poff + filesz:
                return path, file_off - poff + pvaddr
        return path, file_off

    def name(self, addr):
        if addr in self.cache:
            return self.cache[addr]
        i = bisect.bisect_right(self.starts, addr) - 1
        if i < 0 or addr >= self.regions[i][1]:
            result = "?"
        elif not self.regions[i][2]:
            result = self._jit_name(addr)
        elif self.regions[i][2].startswith("["):
            result = self.regions[i][2]
        else:
            path, vaddr = self.vaddr(addr)
            _, addrs, names = self._load(path)
            j = bisect.bisect_right(addrs, vaddr) - 1
            base = os.path.basename(path)
            result = names[j] if j >= 0 else f"{base}+{vaddr:#x}"
            if path != self.exe:
                result = f"{result} ({base})"
        self.cache[addr] = result
        return result


def demangle(names):
    for tool in (["rustfilt"], ["c++filt"]):
        try:
            out = subprocess.run(tool, input="\n".join(names), capture_output=True, text=True).stdout
            lines = out.splitlines()
            if len(lines) == len(names):
                return lines
        except FileNotFoundError:
            continue
    return names


def simplify(name):
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = re.sub(r"\[[0-9a-f]{16}\]", "", name)
    return name.replace("lumen::", "")[:150]


# Approximate rollup by function name; the first matching rule wins.
CATEGORIES = [
    ("jit code", r"^\[jit"),
    ("js parse/compile", r"lumen::parser|lumen::lexer|bytecode::compile|Compiler|scope_analysis|lumen::ast"
                         r"|^parser::|^lexer::"),
    ("html parse", r"html5ever|markup5ever|tokenizer::|tree_builder"),
    ("style/selectors", r"rule_index|computed_cache|prop_index|cascade|Cascade|selector|Selector"
                        r"|matches_compound|match_complex|style_cache|dom::properties|StyleRecord"
                        r"|invalidat|computed_value"),
    ("layout/text", r"trust::layout2|lay_out|Fragment|skrifa|swash|harfrust|shaping|parley|fontique"
                    r"|glyph|trust::text"),
    ("paint/raster", r"paint|vello|raster|trust::render"),
    ("dom/host", r"trust::dom|Dom>::|NodeCache|lumen_backend|trust::js::|host_dom|js_host"),
    ("net/tls/crypto", r"rustls|ring::|aws_lc|h2::|quinn|hyper|tokio|openssl|sha2|aes|chacha|flate"
                       r"|brotli|zstd|trust::http"),
    ("images", r"image::|png::|jpeg|webp|zune|gif::|trust::img"),
    ("jit->rust helper bridge", r"^bytecode::jit_|^bytecode::native_deopt|jit_call_hit|jit_exec"),
    ("calls/frames", r"call_jit|call_native|call_dispatch|call_user|call_inner|call_prepared|run_moved"
                     r"|JitFrame|callback::|resolve_cached_call|call_tail|with_values|Interp>::call\b"
                     r"|jit::run\b|run_compiled_chunk"),
    ("property access", r"get_prop|set_prop|get_member|set_member|get_from_chain|Props>::get|Props>::insert"
                        r"|ic_get|ic_set|shape_transition|finish_cached|get_computed|NamedEntries"),
    ("elements/arrays", r"elem|Dense|packed|mirror|array_|Array"),
    ("alloc/free", r"alloc|malloc|free|fastalloc|new_with_parts|Object>::new|RawVec|finish_grow"),
    ("refcount/drop/pack", r"drop_glue|drop_word|clone_word|PackedValue|into_value|Drop>::drop|release_final"),
    ("hash/side tables", r"hash|Equivalent|hashbrown|weak_|proxy_pair|htmldda|host_indexed|mapped_arg"),
    ("strings/regex", r"lstr|LStr|jstr|regex|memcpy|bcmp|memcmp|units"),
    ("gc", r"gc_|Nursery|obj_refs|registry|collect"),
]


def report(args, samples, threads, sym):
    by_thread = collections.Counter(threads.get(tid, str(tid)) for _ip, tid, _chain in samples)
    if args.thread:
        samples = [s for s in samples if args.thread in threads.get(s[1], str(s[1]))]
    decoded = []
    for ip, _tid, chain in samples:
        frames = [ip] + [a for a in chain if a < PERF_CONTEXT_MAX]
        if len(frames) > 1 and frames[1] == ip:  # the chain repeats the sampled ip
            frames.pop(1)
        decoded.append(frames[:args.depth])
    raw = sorted({sym.name(a) for frames in decoded for a in frames})
    pretty = dict(zip(raw, (simplify(n) for n in demangle(raw))))
    n = max(len(decoded), 1)
    self_c, incl_c = collections.Counter(), collections.Counter()
    callers, stacks = collections.defaultdict(collections.Counter), collections.Counter()
    for frames in decoded:
        names = [pretty[sym.name(a)] for a in frames]
        self_c[names[0]] += 1
        for name in set(names):
            incl_c[name] += 1
        if len(names) > 1:
            callers[names[0]][names[1]] += 1
        stacks[" <- ".join(names[:5])] += 1
    categories = collections.Counter()
    for name, count in self_c.items():
        label = next((label for label, pat in CATEGORIES if re.search(pat, name)), "other")
        categories[label] += count
    with open(args.out, "w") as f:
        total = sum(by_thread.values())
        f.write(f"samples {len(decoded)} of {total} ({args.freq} Hz) exe {sym.exe}"
                f"{f' threads *{args.thread}*' if args.thread else ''}\n\n== threads ==\n")
        for k, v in by_thread.most_common(20):
            f.write(f"{100 * v / max(total, 1):6.2f}% {k}\n")
        f.write("\n== self ==\n")
        for k, v in self_c.most_common(60):
            f.write(f"{100 * v / n:6.2f}% {k}\n")
        f.write("\n== inclusive ==\n")
        for k, v in incl_c.most_common(90):
            f.write(f"{100 * v / n:6.2f}% {k}\n")
        f.write("\n== top self: callers ==\n")
        for k, _ in self_c.most_common(25):
            leading = ", ".join(f"{c} {100 * v / n:.1f}%" for c, v in callers[k].most_common(4))
            f.write(f"{k}\n    <- {leading}\n")
        f.write("\n== top stacks ==\n")
        for k, v in stacks.most_common(40):
            f.write(f"{100 * v / n:6.2f}% {k}\n")
        f.write("\n== categories (self, approximate) ==\n")
        for label, v in categories.most_common():
            f.write(f"{100 * v / n:6.2f}% {label}\n")
        hot = os.environ.get("PSAMPLE_HOT")
        if hot:
            hist = collections.Counter()
            for frames in decoded:
                if hot in pretty[sym.name(frames[0])]:
                    located = sym.vaddr(frames[0])
                    if located:
                        hist[located[1]] += 1
            f.write(f"\n== hot instructions in *{hot}* ==\n")
            for a, v in sorted(hist.items()):
                if v * 1000 >= n:
                    f.write(f"{a:#x} {100 * v / n:6.2f}%\n")
        jithot = os.environ.get("PSAMPLE_JITHOT")
        if jithot:
            hist = collections.Counter()
            for frames in decoded:
                r = sym.jit_range(frames[0])
                if r is not None and jithot in r[2]:
                    hist[(r[2], frames[0] - r[0])] += 1
            f.write(f"\n== hot JIT offsets in *{jithot}* ==\n")
            for (name, off), v in sorted(hist.items()):
                if v * 2000 >= n:
                    f.write(f"{name} +{off:#x} {100 * v / n:6.2f}%\n")
    print("samples", len(decoded))


def main():
    args = parse_args(sys.argv[1:])
    samples, threads, maps_text, exe, jitmap_path = run(args)
    sym = Symbolizer(maps_text, exe, jitmap_path if args.jitmap else None)
    report(args, samples, threads, sym)


if __name__ == "__main__":
    main()
