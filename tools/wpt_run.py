#!/usr/bin/env python3
"""Run web-platform-tests testharness.js tests in trust-headless.

The runner serves a local, read-only WPT checkout with WPT's own server
(`tools/serve`, which supplies the .any.js/.window.js/.worker.js wrappers,
`.sub.` substitutions and Python handlers), loads every selected test in a
separate trust-headless process and collects testharness.js results through a
replacement `/resources/testharnessreport.js`. That report script posts each
completion to a small same-origin Python handler, which writes it to the output
directory; the runner stops the browser as soon as a test's report arrives.

All state (server configuration, report handler, results, logs and Python
bytecode) lives in the output directory and the WPT tree is never written.

The server is configured for `localhost`: subdomains such as
`www.localhost` reach it through the system resolver's `*.localhost` mapping
(nss-myhostname), and `localhost.localdomain` stands in for WPT's alternate
host. HTTPS servers use WPT's own test certificate, which TRust's WebPKI roots
do not trust, so tests are always loaded over HTTP; `http://localhost` is a
secure context, but HTTPS origins and their cross-origin subresources fail.

Example:
    python3 tools/wpt_run.py --wpt ~/wpt --jobs 8 --cpus 0-4,10-14 dom/nodes url
"""

import argparse
import collections
import concurrent.futures
import fnmatch
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

HARNESS_STATUS = {0: "OK", 1: "ERROR", 2: "TIMEOUT", 3: "PRECONDITION_FAILED"}
SUBTEST_STATUS = {0: "PASS", 1: "FAIL", 2: "TIMEOUT", 3: "NOTRUN", 4: "PRECONDITION_FAILED"}
DEFAULT_GLOBALS = ("window", "worker", "sharedworker")
FIREFOX_RUN_INFO = {
    "os": "linux", "os_version": "24.04", "processor": "x86_64", "bits": 64,
    "product": "firefox", "debug": False, "fission": True, "headless": False,
    "display": "x11", "buildapp": "browser", "e10s": True, "nightly_build": True,
    "early_beta_or_earlier": True, "release_or_beta": False, "sessionHistoryInParent": True,
}

# The completion reporter replaces WPT's (empty) default testharnessreport.js.
# It captures what it needs before any test runs, so tests that replace
# XMLHttpRequest or JSON still report.
REPORT_JS = r"""// TRust WPT runner reporter (tools/wpt_run.py); replaces testharnessreport.js.
(function () {
  "use strict";
  var start = String(location.href);
  var XHR = self.XMLHttpRequest, proto = XHR && XHR.prototype;
  var open = proto && proto.open, send = proto && proto.send;
  var setHeader = proto && proto.setRequestHeader;
  var stringify = JSON.stringify, call = Function.prototype.call;
  function text(value) {
    try { return value === null || value === undefined ? null : String(value); }
    catch (e) { return "<unprintable>"; }
  }
  add_completion_callback(function (tests, status) {
    var report = {
      url: start, final_url: text(location.href),
      harness: {status: status.status, message: text(status.message), stack: text(status.stack)},
      tests: []
    };
    for (var i = 0; i < tests.length; i++) {
      report.tests.push({name: text(tests[i].name), status: tests[i].status,
                         message: text(tests[i].message)});
    }
    var body;
    try { body = stringify(report); }
    catch (e) {
      body = stringify({url: start, harness: {status: 1, message: "report failed: " + text(e)},
                        tests: []});
    }
    try {
      var xhr = new XHR();
      call.call(open, xhr, "POST", "/__trust_wpt__/report.py", true);
      call.call(setHeader, xhr, "Content-Type", "text/plain;charset=UTF-8");
      call.call(send, xhr, body);
    } catch (e) {
      try { fetch("/__trust_wpt__/report.py", {method: "POST", body: body}); } catch (e2) {}
    }
  });
  setup({output: false, explicit_timeout: false, timeout_multiplier: %(multiplier)s});
})();
"""

REPORT_PY = r'''# TRust WPT runner report handler (tools/wpt_run.py).
import hashlib
import json
import os
import urllib.parse

RESULTS = %(results)r


def key_for(url):
    parts = urllib.parse.urlsplit(url)
    ident = urllib.parse.unquote(parts.path) + "?" + urllib.parse.unquote(parts.query)
    return hashlib.sha1(ident.encode("utf-8", "surrogatepass")).hexdigest()


def main(request, response):
    try:
        report = json.loads(request.body.decode("utf-8", "replace"))
        key = key_for(report["url"])
    except (ValueError, KeyError, TypeError):
        return 400, [], "bad report"
    temporary = os.path.join(RESULTS, key + ".tmp")
    with open(temporary, "w", encoding="utf-8") as output:
        json.dump(report, output)
    os.replace(temporary, os.path.join(RESULTS, key + ".json"))
    return 200, [("Content-Type", "text/plain")], "ok"
'''

SERVE_BOOTSTRAP = r"""
import sys
root = sys.argv[1]
sys.path[0:0] = [root, root + "/tools"]
import localpaths  # noqa: F401  (adds WPT's third_party packages)
from serve import serve
sys.exit(serve.run(config_path=sys.argv[2], h2=False, webtransport_h3=False,
                   doc_root=None, ws_doc_root=None, ws_extra=[], inject_script=None,
                   alias_file=None, latency=None, exit_after_start=False,
                   verbose=False, report=False, is_wave=False))
"""


def key_for(url):
    parts = urllib.parse.urlsplit(url)
    ident = urllib.parse.unquote(parts.path) + "?" + urllib.parse.unquote(parts.query)
    return hashlib.sha1(ident.encode("utf-8", "surrogatepass")).hexdigest()


def free_ports(count):
    sockets = []
    try:
        for _ in range(count):
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sockets.append(sock)
        return [sock.getsockname()[1] for sock in sockets]
    finally:
        for sock in sockets:
            sock.close()


def import_wpt(root):
    for path in (root / "tools" / "wptrunner", root / "tools", root):
        if str(path) not in sys.path:
            sys.path.insert(0, str(path))
    import localpaths  # noqa: F401
    from manifest.sourcefile import SourceFile
    return SourceFile


class RunInfo(dict):
    def __missing__(self, _key):
        return False


class FirefoxExpectations:
    """Firefox's wptrunner expectations (`meta/`), evaluated for Linux desktop opt."""

    def __init__(self, meta):
        self.meta = meta
        self.run_info = RunInfo(FIREFOX_RUN_INFO)
        self.dirs = {}
        if meta is not None:
            from wptrunner import manifestexpected
            self.manifestexpected = manifestexpected

    def _dir_disabled(self, rel_dir):
        if rel_dir in self.dirs:
            return self.dirs[rel_dir]
        disabled = False
        parent = os.path.dirname(rel_dir)
        if rel_dir and parent != rel_dir:
            disabled = self._dir_disabled(parent)
        ini = self.meta / rel_dir / "__dir__.ini"
        if not disabled and ini.is_file():
            try:
                manifest = self.manifestexpected.get_dir_manifest(str(ini), self.run_info)
                disabled = bool(manifest and manifest.disabled)
            except Exception:  # A malformed expectation file must not stop a run.
                disabled = False
        self.dirs[rel_dir] = disabled
        return disabled

    def lookup(self, source, url):
        """Return {disabled, expected, fail_subtests} or None without metadata."""
        if self.meta is None:
            return None
        record = {"disabled": self._dir_disabled(os.path.dirname(source)), "expected": "OK",
                  "fail_subtests": []}
        try:
            manifest = self.manifestexpected.get_manifest(str(self.meta), source, self.run_info)
        except Exception:
            manifest = None
        test = manifest and manifest.get_test(url.rsplit("/", 1)[1])
        if test is not None:
            record["disabled"] = record["disabled"] or bool(test.disabled)
            if test.has_key("expected"):
                record["expected"] = test.expected
            for name, subtest in test.subtests.items():
                if subtest.has_key("expected") and subtest.expected != "PASS":
                    record["fail_subtests"].append(name)
        return record


def global_kind(url):
    path = urllib.parse.urlsplit(url).path
    for kind in ("serviceworker", "shadowrealm", "sharedworker", "worker"):
        if kind in path.rsplit("/", 1)[-1]:
            return kind
    return "window"


def discover(root, selectors, globals_, excludes):
    SourceFile = import_wpt(root)
    tests = []
    for selector in selectors:
        query = ""
        if "?" in selector:
            selector, query = selector.split("?", 1)
        selector = selector.strip("/")
        base = root / selector
        files = []
        if base.is_dir():
            for directory, dirs, names in os.walk(base):
                dirs[:] = sorted(d for d in dirs if d not in ("resources", "support", "tools"))
                files.extend(os.path.relpath(os.path.join(directory, name), root)
                             for name in sorted(names))
        elif base.is_file():
            files.append(selector)
        else:
            # A test URL such as dom/x.any.worker.html names its .any.js source.
            stem = re.sub(r"\.(any|window|worker)(\.[a-z-]+)?\.html$", "", selector)
            for suffix in (".any.js", ".window.js", ".worker.js"):
                if (root / (stem + suffix)).is_file():
                    files.append(stem + suffix)
                    break
            else:
                raise SystemExit(f"no WPT test or directory: {selector}")
        for rel in files:
            if any(fnmatch.fnmatch(rel, pattern) for pattern in excludes):
                continue
            try:
                kind, items = SourceFile(str(root), rel, "/").manifest_items()
            except Exception:
                continue
            if kind != "testharness":
                continue
            for item in items:
                url = item.url
                if base.is_file() or base.is_dir():
                    if query and urllib.parse.urlsplit(url).query != query:
                        continue
                elif url.lstrip("/").split("?", 1)[0] != selector or (
                        query and urllib.parse.urlsplit(url).query != query):
                    continue
                if global_kind(url) not in globals_:
                    continue
                flags = []
                if item.testdriver:
                    flags.append("testdriver")
                if item.https:
                    flags.append("https")
                if item.subdomain:
                    flags.append("www")
                if item.h2:
                    flags.append("h2")
                tests.append({"url": url, "source": rel, "timeout": item.timeout or "normal",
                              "flags": flags})
    seen = set()
    unique = []
    for test in tests:
        if test["url"] not in seen:
            seen.add(test["url"])
            unique.append(test)
    return unique


def start_server(root, output, multiplier):
    support = output / "support"
    results = output / "reports"
    shutil.rmtree(results, ignore_errors=True)
    support.mkdir(parents=True, exist_ok=True)
    results.mkdir(parents=True)
    (support / "testharnessreport.js").write_text(REPORT_JS % {"multiplier": repr(multiplier)})
    (support / "report.py").write_text(REPORT_PY % {"results": str(results)})
    ports = free_ports(10)
    config = {
        "browser_host": "localhost",
        "alternate_hosts": {"alt": "localhost.localdomain"},
        "server_host": "127.0.0.1",
        "doc_root": str(root),
        "ws_doc_root": str(root / "websockets" / "handlers"),
        "check_subdomains": False,
        "bind_address": True,
        "ports": {
            "http": ports[0:2], "https": ports[2:4], "http-private": [ports[4]],
            "http-public": [ports[5]], "https-private": [ports[6]], "https-public": [ports[7]],
            "ws": [ports[8]], "wss": [ports[9]],
        },
        "aliases": [
            {"url-path": "/resources/testharnessreport.js", "local-dir": str(support)},
            {"url-path": "/__trust_wpt__/", "local-dir": str(support)},
        ],
        "logging": {"level": "WARNING", "suppress_handler_traceback": False},
    }
    config_path = output / "wpt-config.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    env = dict(os.environ, PYTHONPYCACHEPREFIX=str(output / "pycache"), PYTHONDONTWRITEBYTECODE="1")
    log = (output / "server.log").open("w")
    process = subprocess.Popen([sys.executable, "-c", SERVE_BOOTSTRAP, str(root), str(config_path)],
                               stdout=log, stderr=subprocess.STDOUT, env=env,
                               start_new_session=True)
    origin = f"http://localhost:{ports[0]}"
    deadline = time.monotonic() + 60
    while True:
        if process.poll() is not None:
            raise SystemExit(f"WPT server exited; see {output / 'server.log'}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{ports[0]}/resources/testharnessreport.js",
                                        timeout=2) as response:
                if b"TRust WPT runner" in response.read():
                    break
        except (urllib.error.URLError, OSError):
            pass
        if time.monotonic() > deadline:
            stop_server(process)
            raise SystemExit("WPT server did not start within 60 s")
        time.sleep(0.2)
    return process, origin, results


def stop_server(process):
    if process.poll() is None:
        try:
            # serve.run() shuts its server processes down on KeyboardInterrupt.
            process.send_signal(signal.SIGINT)
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()


JS_ERROR_LINE = re.compile(r"^\s*(?:[-*]\s*)?(.*)$")


def parse_js_errors(log_text, limit=6):
    """Collect the first lines of trust-headless's [js-errors] block."""
    errors = []
    active = False
    for line in log_text.splitlines():
        if line.startswith("[js-errors]"):
            active = True
            continue
        if active:
            if line.startswith("[") and not line.startswith("[ "):
                active = False
                continue
            line = line.strip()
            if line:
                errors.append(line[:300])
                if len(errors) >= limit:
                    break
    return errors


def run_test(test, args, origin, results_dir, logs_dir):
    origin_parts = urllib.parse.urlsplit(origin)
    host = f"www.localhost:{origin_parts.port}" if "www" in test["flags"] else origin_parts.netloc
    url = f"http://{host}{test['url']}"
    harness_timeout = (60 if test["timeout"] == "long" else 10) * args.timeout_multiplier
    limit = harness_timeout + args.grace
    report_path = results_dir / (key_for(test["url"]) + ".json")
    if report_path.exists():
        report_path.unlink()
    command = [str(args.binary), "--timeout", str(int(limit) + 1), "--settle", "0",
               "--js-diagnostics", "--max-chars", "200", url]
    if args.cpus:
        command = ["taskset", "-c", args.cpus] + command
    log_path = logs_dir / (key_for(test["url"]) + ".log")
    started = time.monotonic()
    with log_path.open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log, start_new_session=True)
        deadline = started + limit + 5
        report = None
        while time.monotonic() < deadline:
            if report_path.exists():
                break
            if process.poll() is not None:
                time.sleep(0.2)  # A report can land just after the process exits.
                break
            time.sleep(0.05)
        exit_code = process.poll()
        if exit_code is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
    duration = time.monotonic() - started
    if report_path.exists():
        try:
            report = json.loads(report_path.read_text())
        except ValueError:
            report = None
    log_text = log_path.read_text(errors="replace")
    record = {"url": test["url"], "source": test["source"], "flags": test["flags"],
              "timeout": test["timeout"], "duration": round(duration, 2), "exit_code": exit_code}
    if report is None:
        if exit_code is None:
            record["status"] = "NO_RESULT_TIMEOUT"
        elif exit_code < 0 or exit_code > 3 or "panicked" in log_text:
            record["status"] = "CRASH"
        else:
            record["status"] = "NO_RESULT"
        record["message"] = None
        record["subtests"] = []
    else:
        harness = report.get("harness") or {}
        record["status"] = HARNESS_STATUS.get(harness.get("status"), "ERROR")
        record["message"] = harness.get("message")
        record["subtests"] = [
            {"name": sub.get("name"), "status": SUBTEST_STATUS.get(sub.get("status"), "FAIL"),
             "message": sub.get("message")}
            for sub in report.get("tests") or []
        ]
    record["js_errors"] = parse_js_errors(log_text)
    keep_log = record["status"] != "OK" or any(s["status"] != "PASS" for s in record["subtests"])
    if not keep_log:
        log_path.unlink()
    return record


def subtest_counts(record):
    counts = collections.Counter(sub["status"] for sub in record["subtests"])
    return counts


MESSAGE_SUBSTITUTIONS = [
    (re.compile(r'"(?:[^"\\]|\\.)*"'), '"…"'),
    (re.compile(r"'(?:[^'\\]|\\.)*'"), "'…'"),
    (re.compile(r"\b\d+(\.\d+)?\b"), "N"),
]
API_PATTERNS = [
    ("not defined", re.compile(r"\b([A-Za-z_$][\w$]*) is not defined")),
    ("not a function", re.compile(r"([A-Za-z_$][\w$.\[\]]*) is not a function")),
    ("not a constructor", re.compile(r"([A-Za-z_$][\w$.]*) is not a constructor")),
    ("missing property", re.compile(r'expected property "([^"]+)" missing')),
    ("missing property", re.compile(r'property "([^"]+)" (?:not found|missing)')),
    ("undefined property read", re.compile(r"(?:Cannot read propert(?:y|ies) of undefined|"
                                           r"undefined is not an object)[^\n]{0,60}")),
]


def normalize_message(message):
    message = (message or "").strip().splitlines()[0] if message else ""
    for pattern, replacement in MESSAGE_SUBSTITUTIONS:
        message = pattern.sub(replacement, message)
    return message[:160]


def group_of(source, depth):
    if isinstance(depth, (list, tuple)):
        # Group by the longest requested path that contains the source.
        matches = [path for path in depth if source == path or source.startswith(path.rstrip("/") + "/")]
        if matches:
            return max(matches, key=len)
        depth = 2
    parts = source.split("/")[:-1]
    return "/".join(parts[:depth]) if parts else "(root)"


def summarize(records, depth, firefox_aware=True):
    groups = collections.OrderedDict()
    for record in sorted(records, key=lambda r: r["url"]):
        group = groups.setdefault(group_of(record["source"], depth), collections.Counter())
        group["tests"] += 1
        group["harness_" + record["status"]] += 1
        counts = subtest_counts(record)
        group["subtests"] += len(record["subtests"])
        group["pass"] += counts["PASS"]
        group["fail"] += counts["FAIL"]
        group["timeout"] += counts["TIMEOUT"]
        group["notrun"] += counts["NOTRUN"]
        ff = record.get("firefox") or {}
        fail_names = set(ff.get("fail_subtests") or [])
        for sub in record["subtests"]:
            if sub["status"] != "PASS" and sub["name"] not in fail_names:
                group["fail_firefox_passes"] += 1
    messages = collections.Counter()
    apis = collections.Counter()
    api_examples = {}
    for record in records:
        texts = [sub["message"] for sub in record["subtests"] if sub["status"] != "PASS"]
        if record["status"] not in ("OK",):
            texts.append(record.get("message"))
            texts.extend(record.get("js_errors") or [])
        for text in texts:
            if not text:
                continue
            messages[normalize_message(text)] += 1
            for kind, pattern in API_PATTERNS:
                for match in pattern.finditer(text):
                    key = f"{kind}: {match.group(1) if pattern.groups else match.group(0)}"
                    apis[key] += 1
                    api_examples.setdefault(key, record["url"])
    return groups, messages, apis, api_examples


def format_matrix(groups):
    lines = ["| directory | tests | subtests pass/total | % | harness OK | ERROR | TIMEOUT | no result/crash | sub TIMEOUT | NOTRUN | fail where Firefox passes |",
             "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    total = collections.Counter()
    for name, group in groups.items():
        total.update(group)
        lines.append(format_row(name, group))
    lines.append(format_row("**total**", total))
    return "\n".join(lines)


def format_row(name, group):
    no_result = group["harness_NO_RESULT"] + group["harness_NO_RESULT_TIMEOUT"] + group["harness_CRASH"]
    percent = 100.0 * group["pass"] / group["subtests"] if group["subtests"] else 0.0
    return (f"| {name} | {group['tests']} | {group['pass']}/{group['subtests']} | {percent:.1f} | "
            f"{group['harness_OK']} | {group['harness_ERROR']} | {group['harness_TIMEOUT']} | "
            f"{no_result} | {group['timeout']} | {group['notrun']} | {group['fail_firefox_passes']} |")


def write_summary(records, output, depth, baseline=None):
    groups, messages, apis, api_examples = summarize(records, depth)
    report = ["# TRust WPT results", "", format_matrix(groups), ""]
    if baseline:
        base_groups, *_ = summarize(baseline, depth)
        report += ["## Change from baseline", "",
                   "| directory | subtests passed (before → after) | harness OK (before → after) |",
                   "|---|---:|---:|"]
        for name in sorted(set(groups) | set(base_groups)):
            new, old = groups.get(name, collections.Counter()), base_groups.get(name, collections.Counter())
            if (new["pass"], new["subtests"], new["harness_OK"]) != (old["pass"], old["subtests"], old["harness_OK"]):
                report.append(f"| {name} | {old['pass']}/{old['subtests']} → {new['pass']}/{new['subtests']} | "
                              f"{old['harness_OK']} → {new['harness_OK']} |")
        report.append("")
    report += ["## Missing-API signals", "", "| count | signal | first test |", "|---:|---|---|"]
    for key, count in apis.most_common(60):
        report.append(f"| {count} | `{key}` | {api_examples[key]} |")
    report += ["", "## Most frequent failure messages", "", "| count | message |", "|---:|---|"]
    for message, count in messages.most_common(80):
        report.append(f"| {count} | `{message.replace('|', '¦')}` |")
    report += ["", "## Tests without a result", ""]
    for record in records:
        if record["status"] in ("NO_RESULT", "NO_RESULT_TIMEOUT", "CRASH"):
            detail = "; ".join(record.get("js_errors") or [])[:200]
            report.append(f"- {record['status']} {record['url']} {detail}")
    text = "\n".join(report) + "\n"
    (output / "summary.md").write_text(text)
    summary = {
        "groups": groups, "top_messages": messages.most_common(200),
        "api_signals": apis.most_common(200),
    }
    (output / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    return format_matrix(groups)


def load_records(path):
    with open(path) as handle:
        return [json.loads(line) for line in handle if line.strip()]


def main():
    default_target = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("paths", nargs="*", help="WPT-relative directories, files or test URLs")
    parser.add_argument("--wpt", type=pathlib.Path, default=os.environ.get("TRUST_WPT_ROOT"),
                        help="WPT checkout (default: $TRUST_WPT_ROOT)")
    parser.add_argument("--meta", type=pathlib.Path,
                        help="Firefox expectation metadata (default: WPT/../meta when present)")
    parser.add_argument("--binary", type=pathlib.Path, default=default_target / "release" / "trust-headless")
    parser.add_argument("--output", type=pathlib.Path, default=default_target / "wpt-results")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--cpus", help="taskset CPU list for browser processes, e.g. 0-4,10-14")
    parser.add_argument("--globals", default=",".join(DEFAULT_GLOBALS),
                        help="comma-separated test globals: window,worker,sharedworker,serviceworker,shadowrealm")
    parser.add_argument("--exclude", action="append", default=[], help="fnmatch pattern on source paths")
    parser.add_argument("--skip-testdriver", action="store_true",
                        help="skip tests that need WebDriver actions (testdriver.js)")
    parser.add_argument("--skip-firefox-disabled", action="store_true",
                        help="skip tests Firefox's metadata disables on Linux")
    parser.add_argument("--timeout-multiplier", type=float, default=1.0,
                        help="testharness timeout_multiplier (normal 10 s, long 60 s)")
    parser.add_argument("--grace", type=float, default=8.0,
                        help="seconds beyond the harness timeout before the browser is killed")
    parser.add_argument("--depth", type=int, default=2,
                        help="directory depth for the summary matrix; 0 groups by the requested paths")
    parser.add_argument("--baseline", type=pathlib.Path, help="earlier results.jsonl to compare against")
    parser.add_argument("--list", action="store_true", help="list the selected test URLs and exit")
    parser.add_argument("--serve", action="store_true",
                        help="only start the configured WPT server and wait, for manual debugging")
    parser.add_argument("--summarize", type=pathlib.Path,
                        help="rewrite the summary for an existing results.jsonl and exit")
    args = parser.parse_args()
    baseline = load_records(args.baseline) if args.baseline else None
    if args.summarize:
        output = args.summarize.resolve().parent
        depth = args.depth
        if depth == 0:
            try:
                depth = [path.split("?", 1)[0].strip("/")
                         for path in json.loads((output / "run.json").read_text())["paths"]]
            except (OSError, ValueError, KeyError):
                depth = 2
        print(write_summary(load_records(args.summarize), output, depth, baseline))
        return 0
    if args.wpt is None or not (args.wpt / "resources" / "testharness.js").is_file():
        parser.error("pass --wpt or set TRUST_WPT_ROOT to a web-platform-tests checkout")
    root = args.wpt.resolve()
    if args.serve:
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        server, origin, _results = start_server(root, output, args.timeout_multiplier)
        print(f"serving {root} at {origin} (Ctrl+C stops)", flush=True)
        try:
            server.wait()
        except KeyboardInterrupt:
            pass
        finally:
            stop_server(server)
        return 0
    if not args.paths:
        parser.error("name at least one WPT directory, file or test URL")
    meta = args.meta or (root.parent / "meta")
    meta = meta.resolve() if meta.is_dir() else None
    globals_ = set(args.globals.split(","))
    os.environ.setdefault("PYTHONPYCACHEPREFIX", str(args.output.resolve() / "pycache"))
    sys.dont_write_bytecode = True
    tests = discover(root, args.paths, globals_, args.exclude)
    expectations = FirefoxExpectations(meta)
    selected = []
    for test in tests:
        test["firefox"] = expectations.lookup(test["source"], test["url"])
        if args.skip_testdriver and "testdriver" in test["flags"]:
            continue
        if args.skip_firefox_disabled and test["firefox"] and test["firefox"]["disabled"]:
            continue
        selected.append(test)
    if args.list:
        for test in selected:
            print(test["url"], " ".join(test["flags"]))
        print(f"{len(selected)} tests", file=sys.stderr)
        return 0
    if not args.binary.is_file():
        parser.error(f"missing trust-headless binary: {args.binary}")
    args.binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    logs_dir = output / "logs"
    shutil.rmtree(logs_dir, ignore_errors=True)
    logs_dir.mkdir()
    server, origin, results_dir = start_server(root, output, args.timeout_multiplier)
    print(f"serving {root} at {origin}; {len(selected)} tests, {args.jobs} jobs", flush=True)
    records = []
    lock = threading.Lock()
    started = time.monotonic()
    jsonl = (output / "results.jsonl").open("w")

    def task(test):
        record = run_test(test, args, origin, results_dir, logs_dir)
        record["firefox"] = test["firefox"]
        with lock:
            records.append(record)
            jsonl.write(json.dumps(record) + "\n")
            jsonl.flush()
            counts = subtest_counts(record)
            print(f"[{len(records)}/{len(selected)}] {record['status']:<17} "
                  f"{counts['PASS']}/{len(record['subtests'])} {record['url']} ({record['duration']}s)",
                  flush=True)
        return record

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            list(pool.map(task, selected))
    except KeyboardInterrupt:
        print("interrupted; summarizing completed tests", file=sys.stderr)
    finally:
        jsonl.close()
        stop_server(server)
    meta_info = {"wpt": str(root), "meta": str(meta) if meta else None, "binary": str(args.binary),
                 "paths": args.paths, "globals": sorted(globals_), "jobs": args.jobs,
                 "timeout_multiplier": args.timeout_multiplier,
                 "elapsed_seconds": round(time.monotonic() - started, 1)}
    (output / "run.json").write_text(json.dumps(meta_info, indent=2) + "\n")
    depth = [path.split("?", 1)[0].strip("/") for path in args.paths] if args.depth == 0 else args.depth
    print(write_summary(records, output, depth, baseline))
    print(f"elapsed {meta_info['elapsed_seconds']} s; results in {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
