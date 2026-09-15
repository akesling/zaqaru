"""Serve a waiting page immediately, then expose the validated local demo."""

import argparse
import functools
import html
import http.server
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time


def main():
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="Serve the cached demo; build it when missing or stale.")
    parser.add_argument("port", nargs="?", type=int, default=8000)
    parser.add_argument("--rebuild", action="store_true", help="rebuild even when the demo is cached")
    args = parser.parse_args()
    port = args.port
    if not 1 <= port <= 65535:
        parser.error("port must be between 1 and 65535")
    ready = threading.Event()
    started = time.monotonic()
    progress_lock = threading.Lock()
    progress = {"stage": "Starting the build", "since": started, "detail": ""}

    def status_text():
        with progress_lock:
            now = time.monotonic()
            return (progress["stage"], progress["detail"],
                    f'{int(now - progress["since"])} s in this stage; '
                    f'{int(now - started)} s total')

    def relay_output(stream):
        for line in stream:
            print(line, end="", flush=True)
            with progress_lock:
                if line.startswith("demo-progress: "):
                    progress.update(stage=line.removeprefix("demo-progress: ").strip(),
                                    since=time.monotonic(), detail="")
                elif line.startswith("preboot: "):
                    progress["detail"] = line.strip()

    class Handler(http.server.SimpleHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/favicon.ico":
                self.send_response(204)
                self.end_headers()
                return
            if ready.is_set():
                return super().do_GET()
            stage, detail, elapsed = status_text()
            page = (
                '<!doctype html><meta charset="utf-8">'
                '<meta http-equiv="refresh" content="3">'
                '<title>Preparing Zaqaru</title><h1>Preparing the demo…</h1>'
                f'<p><strong>{html.escape(stage)}</strong></p>'
                f'<p>{html.escape(elapsed)}</p><p>{html.escape(detail)}</p>'
                '<p>This page will reload automatically when ready. '
                'Full build output is printed in your terminal.</p>'
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(page)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(page)

        def log_request(self, code="-", size="-"):
            # Waiting-page refreshes otherwise drown out actual build progress.
            if ready.is_set() and self.path != "/favicon.ico":
                super().log_request(code, size)

    try:
        server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", port), functools.partial(Handler, directory=str(repo))
        )
    except OSError as error:
        print(f"Cannot serve on port {port}: {error}", file=sys.stderr)
        return 1
    url = (f"http://127.0.0.1:{port}/web/?module=demo/hello-django.wasm"
           "&snapshot=demo/hello-django.snapshot&live=80")
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"\nServing on port {port}\nOpen {url}", flush=True)
    print("Preparing demo assets; the page will reload when ready. Ctrl-C stops the server.\n", flush=True)
    build = None
    try:
        command = ["sh", str(repo / "web/demo.sh")]
        if args.rebuild:
            command.append("--rebuild")
        build = subprocess.Popen(command, start_new_session=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 text=True, errors="replace")
        output = threading.Thread(target=relay_output, args=(build.stdout,), daemon=True)
        output.start()
        while True:
            try:
                status = build.wait(timeout=10)
                break
            except subprocess.TimeoutExpired:
                stage, detail, elapsed = status_text()
                print(f"demo: {stage} ({elapsed})", flush=True)
        output.join()
        if status:
            print(f"Demo build failed (exit {status}); stopping server.", file=sys.stderr)
            return 1
        ready.set()
        print(f"\nDemo ready: {url}\nPress Send in the page to make a request.", flush=True)
        threading.Event().wait()
    except KeyboardInterrupt:
        return 0
    finally:
        if build is not None and build.poll() is None:
            os.killpg(build.pid, signal.SIGTERM)
            build.wait()
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    sys.exit(main())
