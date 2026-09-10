#!/usr/bin/env python3
"""Run a pinned CSS WPT subset in trust-headless; fail on any failure or timeout.

Uses upstream testharness.js and assertions, with a local completion reporter.
This is a focused standalone runner, not full WPT/wptrunner product integration.
"""

import argparse
import concurrent.futures
import functools
import hashlib
import http.server
import json
import pathlib
import subprocess
import threading
import time
import urllib.request


REVISION = "7c3bae55cab5840dc333e99ffd56d5422811ce2c"
TESTS = [
    "css/css-properties-values-api/" + name + ".html"
    for name in (
        "registered-property-computation", "registered-properties-inheritance",
        "registered-property-crosstalk", "registered-property-initial",
        "registered-property-dependency-through-fallback", "invalid-at-computed-value-time",
        "unit-cycles", "non-computed-unit-cycles", "var-reference-registered-properties-cycles",
        "var-reference-unit-cycles", "registered-property-cssom", "get-computed-style-enumeration",
        "at-property-stylesheets", "register-property",
    )
] + [
    "css/css-variables/variable-cycles.html",
    "css/css-backgrounds/parsing/background-shorthand-serialization.html",
]
RESOURCES = [
    "resources/testharness.js", "resources/testharnessreport.js",
    "css/css-properties-values-api/resources/utils.js",
]
REPORTER = b"""
;add_completion_callback(function(tests,status){
 var result={url:location.href,harness:{status:status.status,message:status.message},
   tests:tests.map(function(t){return {name:t.name,status:t.status,message:t.message};})};
 var xhr=new XMLHttpRequest();xhr.open('POST','/__results/'+location.search.slice(1));
 xhr.setRequestHeader('Content-Type','application/json');xhr.send(JSON.stringify(result));
});
"""


class Server(http.server.ThreadingHTTPServer):
    def __init__(self, root):
        self.results = {}
        self.results_lock = threading.Lock()
        self.root = root
        super().__init__(("127.0.0.1", 0), functools.partial(Handler, directory=str(root)))


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path.split("?", 1)[0] != "/resources/testharness.js":
            return super().do_GET()
        data = (self.server.root / "resources/testharness.js").read_bytes() + REPORTER
        self.send_response(200)
        self.send_header("Content-Type", "text/javascript")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        key = self.path.removeprefix("/__results/")
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not self.path.startswith("/__results/") or not key.isdecimal() or not 0 < length <= 8_388_608:
                raise ValueError("invalid completion report")
            result = json.loads(self.rfile.read(length))
            if not isinstance(result.get("tests"), list) or not isinstance(result.get("harness"), dict):
                raise ValueError("invalid result structure")
        except (ValueError, TypeError, AttributeError):
            self.send_error(400)
            return
        with self.server.results_lock:
            self.server.results[key] = result
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default=pathlib.Path("target/release/trust-headless"))
    parser.add_argument("--cache", type=pathlib.Path, default=pathlib.Path("target/css-wpt-cache"))
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("target/css-wpt-results"))
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--jobs", type=int, default=3)
    parser.add_argument("--offline", action="store_true", help="require previously downloaded sources")
    args = parser.parse_args()
    if args.timeout <= 0 or args.jobs <= 0 or not args.binary.is_file():
        parser.error("provide an existing release binary and positive timeout/jobs")
    digest = hashlib.sha256()
    with args.binary.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(chunk)
    root = args.cache.resolve() / REVISION
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    for path in TESTS + RESOURCES:
        dest = root / path
        if dest.is_file():
            continue
        if args.offline:
            parser.error(f"missing cached WPT source: {dest}")
        dest.parent.mkdir(parents=True, exist_ok=True)
        url = f"https://raw.githubusercontent.com/web-platform-tests/wpt/{REVISION}/{path}"
        with urllib.request.urlopen(url, timeout=30) as response:
            data = response.read()
        temporary = dest.with_suffix(dest.suffix + ".tmp")
        temporary.write_bytes(data)
        temporary.replace(dest)
    server = Server(root)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    def run(item):
        index, path = item
        key = str(index)
        url = f"http://127.0.0.1:{server.server_port}/{path}?{key}"
        with (output / f"{index:02}.log").open("w") as log:
            process = subprocess.Popen([
                str(args.binary.resolve()), "--timeout", str(args.timeout), "--settle", "0",
                "--js-diagnostics", "--max-chars", "500", url,
            ], stdout=log, stderr=log)
            result = None
            try:
                deadline = time.monotonic() + args.timeout + 5
                while time.monotonic() < deadline:
                    with server.results_lock:
                        result = server.results.get(key)
                    if result is not None or process.poll() is not None:
                        break
                    time.sleep(0.05)
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            # A report can arrive between polling and process exit.
            with server.results_lock:
                result = server.results.get(key, result)
        complete = result is not None and result["harness"].get("status") == 0 and bool(result["tests"])
        passed = sum(test.get("status") == 0 for test in result["tests"]) if result else 0
        failed = len(result["tests"]) - passed if result else 0
        record = {"test": path, "complete": complete, "passed": passed, "failed": failed, "result": result}
        (output / f"{index:02}.json").write_text(json.dumps(record, indent=2) + "\n")
        print(f"{path}: {passed} PASS / {failed} FAIL" + ("" if complete else " / INCOMPLETE"), flush=True)
        return record

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            results = list(pool.map(run, enumerate(TESTS)))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    summary = {
        "revision": REVISION, "binary": str(args.binary.resolve()),
        "binary_sha256": digest.hexdigest(),
        "passed": sum(result["passed"] for result in results),
        "failed": sum(result["failed"] for result in results),
        "incomplete": sum(not result["complete"] for result in results),
        "tests": results,
    }
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"Total: {summary['passed']} PASS / {summary['failed']} FAIL / "
          f"{summary['incomplete']} INCOMPLETE pages", flush=True)
    return int(any(not result["complete"] or result["failed"] for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
