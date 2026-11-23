"""End-to-end checks using a real proxy process and a controlled HTTP origin."""

import concurrent.futures
import contextlib
import http.client
import http.server
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time
import threading
import unittest


BINARY = Path(os.environ.get("RPC_TEST_BINARY", "target/debug/rs-proxy-cache")).resolve()
SIGNED_PATH = "/video%2Emp4?token=a%2Fb&expires=123"


class Origin(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self.reply()

    def do_GET(self):
        self.reply()

    def reply(self):
        state = self.server
        requested = self.headers.get("Range")
        state.requests.append((self.command, self.path, requested))
        data, etag = state.data, state.etag
        if self.path == "/playable.mp4?sig=a%2Fb&z=1":
            data, etag = state.mp4, '"mp4"'
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", SIGNED_PATH)
            self.end_headers()
            return
        if self.path == "/denied-redirect":
            self.send_response(302)
            self.send_header("Location", f"http://localhost:{state.server_port}{SIGNED_PATH}")
            self.end_headers()
            return
        if state.expired:
            self.send_error(403)
            return

        total = len(data)
        status, start, end = 200, 0, total
        if requested and not state.ignore_range:
            left, right = re.fullmatch(r"bytes=(\d*)-(\d*)", requested).groups()
            start = int(left) if left else max(0, total - int(right))
            end = min(total, int(right) + 1) if left and right else total
            if start >= end:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{total}")
                self.end_headers()
                return
            status = 206

        self.send_response(status)
        self.send_header("Content-Type", "video/mp4")
        self.send_header("Content-Length", str(end - start))
        if etag is not None:
            self.send_header("ETag", etag)
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end - 1}/{total}")
        self.end_headers()
        if self.command == "GET":
            if state.delay:
                time.sleep(state.delay)
            try:
                if state.block_range and requested == state.block_range:
                    state.release_range.wait(5)
                if state.truncate:
                    self.wfile.write(data[start:(start + end) // 2])
                    self.close_connection = True
                else:
                    self.wfile.write(data[start:end])
            except (BrokenPipeError, ConnectionResetError):
                pass


class PlaybackTests(unittest.TestCase):
    def setUp(self):
        self.root = self.enterContext(tempfile.TemporaryDirectory())
        self.cache = Path(self.root) / "cache"
        origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
        self.origin = origin
        self.addCleanup(origin.server_close)
        threading.Thread(target=origin.serve_forever, daemon=True).start()
        self.addCleanup(origin.shutdown)
        origin.requests = []
        origin.data = bytes(range(200))
        origin.etag = '"one"'
        origin.expired = origin.ignore_range = origin.truncate = False
        origin.delay = 0
        origin.block_range = None
        origin.release_range = threading.Event()
        self.source = f"http://127.0.0.1:{origin.server_port}"
        self.proxy = None
        self.addCleanup(self.stop)

    def start(self, capacity=1024, private=True):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.proxy = subprocess.Popen(
            [BINARY],
            env={**os.environ, "RPC_LISTEN": f"127.0.0.1:{self.port}",
                 "RPC_ORIGIN_HOSTS": "127.0.0.1",
                 "RPC_ALLOW_PRIVATE_ORIGINS": str(private).lower(),
                 "RPC_CACHE_DIR": str(self.cache), "RPC_CACHE_BYTES": str(capacity)},
            stderr=subprocess.PIPE,
        )
        line = self.proxy.stderr.readline().decode()
        self.assertIn("listening on", line)

    def stop(self):
        if self.proxy is not None:
            self.proxy.terminate()
            self.proxy.communicate(timeout=5)
            self.proxy = None

    def get(self, spec=None, path=SIGNED_PATH, method="GET", headers_received=None):
        headers = {"Range": f"bytes={spec}"} if spec is not None else {}
        with contextlib.closing(http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)) as conn:
            conn.request(method, "/v1/" + self.source + path, headers=headers)
            response = conn.getresponse()
            if headers_received is not None:
                headers_received.set()
            return response.status, response.headers, response.read()

    def test_ranges_hits_restart_expiry_and_clear(self):
        self.start()
        status, headers, body = self.get("80-99")
        self.assertEqual((status, headers["Content-Range"], body),
                         (206, "bytes 80-99/200", self.origin.data[80:100]))
        self.assertEqual(self.origin.requests, [("GET", SIGNED_PATH, "bytes=80-99")])
        self.assertEqual(self.get("90-94")[2], self.origin.data[90:95])
        self.assertEqual(self.get(method="HEAD")[1]["Content-Length"], "200")
        self.stop()
        self.origin.expired = True
        self.start()
        self.assertEqual(self.get("80-99")[2], self.origin.data[80:100])
        self.assertEqual(len(self.origin.requests), 1)
        self.assertEqual(self.get("75-90")[0], 403)
        self.assertEqual(self.origin.requests[-1][2], "bytes=75-79")
        self.stop()
        subprocess.run([BINARY, "clear", self.source + SIGNED_PATH], check=True,
                       env={**os.environ, "RPC_CACHE_DIR": str(self.cache)})
        self.start()
        self.assertEqual(self.get("80-99")[0], 403)

    def test_fill_only_gaps_then_replace_representation(self):
        self.start()
        self.get("40-59")
        self.assertEqual(self.get("30-69")[2], self.origin.data[30:70])
        self.assertEqual([item[2] for item in self.origin.requests],
                         ["bytes=40-59", "bytes=30-39", "bytes=60-69"])
        self.origin.etag = '"two"'
        self.origin.data = bytes(reversed(range(150)))
        self.assertEqual(self.get("40-199")[2], self.origin.data[40:])
        self.assertEqual(self.get("-10")[2], self.origin.data[-10:])
        self.assertEqual(len(self.origin.requests), 5)

    def test_persistent_lru(self):
        self.start(capacity=40)
        self.get("0-19")
        self.get("20-39")
        self.get("0-19")
        self.stop()
        self.start(capacity=40)
        self.get("40-59")
        self.get("0-19")
        self.assertEqual(len(self.origin.requests), 3)
        self.get("20-39")
        self.assertEqual(len(self.origin.requests), 4)

    def test_concurrent_identical_ranges(self):
        self.start()
        self.origin.delay = 0.2
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(self.get, ["40-59", "40-59"]))
        self.assertEqual([result[2] for result in results], [self.origin.data[40:60]] * 2)
        self.assertEqual(len(self.origin.requests), 1)

    def test_seek_does_not_wait_for_existing_download(self):
        self.start()
        self.origin.block_range = "bytes=0-99"
        headers_received = threading.Event()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(self.get, "0-99", headers_received=headers_received)
            try:
                self.assertTrue(headers_received.wait(3))
                second = pool.submit(self.get, "150-159")
                self.assertEqual(second.result(timeout=3)[2], self.origin.data[150:160])
            finally:
                self.origin.release_range.set()
            self.assertEqual(first.result(timeout=3)[2], self.origin.data[:100])

    def test_inflight_old_version_cannot_repopulate_cache(self):
        self.start()
        self.origin.block_range = "bytes=0-99"
        original = self.origin.data
        headers_received = threading.Event()
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            first = pool.submit(self.get, "0-99", headers_received=headers_received)
            try:
                self.assertTrue(headers_received.wait(3))
                self.origin.etag = '"two"'
                self.origin.data = bytes(reversed(range(200)))
                self.assertEqual(self.get("150-159")[2], self.origin.data[150:160])
            finally:
                self.origin.release_range.set()
            self.assertEqual(first.result(timeout=3)[2], original[:100])
        self.assertEqual(self.get("0-99")[2], self.origin.data[:100])
        self.assertEqual(len(self.origin.requests), 3)

    def test_head_whole_and_ignored_ranges(self):
        self.start()
        self.assertEqual(self.get(method="HEAD")[1]["Content-Length"], "200")
        status, _, body = self.get()
        self.assertEqual((status, body), (200, self.origin.data))
        self.assertEqual(self.get("199-")[2], self.origin.data[-1:])
        self.assertEqual(self.get("200-")[0], 416)
        self.assertEqual(self.get("1-2,4-5")[0], 400)
        self.origin.ignore_range = True
        self.assertEqual(self.get("1-2", path="/other")[0], 502)
        self.assertEqual(self.get(path="/other")[2], self.origin.data)

    def test_no_strong_etag_fetches_whole_request_on_partial_hit(self):
        self.start()
        self.origin.etag = 'W/"one"'
        self.get("40-59")
        self.get("45-54")
        self.assertEqual(len(self.origin.requests), 1)
        self.get("30-49")
        self.assertEqual(self.origin.requests[-1][2], "bytes=30-49")

    def test_truncated_body_is_not_published(self):
        self.start()
        self.origin.truncate = True
        with self.assertRaises(http.client.IncompleteRead):
            self.get("40-59")
        self.origin.truncate = False
        self.assertEqual(self.get("40-59")[2], self.origin.data[40:60])
        self.assertEqual(len(self.origin.requests), 2)

    def test_redirect_is_checked_and_private_addresses_are_blocked_by_default(self):
        self.start(private=False)
        self.assertEqual(self.get("0-9")[0], 502)
        self.assertEqual(self.origin.requests, [])
        self.stop()
        self.start()
        self.assertEqual(self.get("0-9", path="/redirect")[2], self.origin.data[:10])
        self.assertEqual(self.get("0-9", path="/denied-redirect")[0], 502)
        self.assertEqual([item[1] for item in self.origin.requests],
                         ["/redirect", SIGNED_PATH, "/denied-redirect"])

    @unittest.skipUnless(shutil.which("ffmpeg"), "ffmpeg is optional")
    def test_ffmpeg_decodes_and_seeks_a_real_mp4(self):
        self.start(capacity=2 * 1024 * 1024)
        mp4 = Path(self.root) / "generated.mp4"
        subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                        "testsrc2=size=640x360:rate=24", "-t", "12", "-c:v", "mpeg4",
                        "-g", "12", "-q:v", "3", "-movflags", "+faststart", str(mp4)], check=True, timeout=20)
        self.origin.mp4 = mp4.read_bytes()
        path = "/playable.mp4?sig=a%2Fb&z=1"
        url = f"http://127.0.0.1:{self.port}/v1/{self.source}{path}"
        subprocess.run(["ffmpeg", "-v", "error", "-ss", "9", "-short_seek_size", "0", "-i", url,
                        "-frames:v", "4", "-f", "null", "-"], check=True, timeout=20)
        self.assertTrue(all(item[1] == path for item in self.origin.requests))
        self.assertTrue(any(item[2] and item[2] != "bytes=0-" for item in self.origin.requests))


if __name__ == "__main__":
    unittest.main()
