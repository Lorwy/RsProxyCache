"""Compare cold FFmpeg decoding with and without concurrent offline downloads."""

import argparse
import json
from pathlib import Path
import statistics
import subprocess
import tempfile
import threading
import time

from playback import FFMPEG, Origin, PlaybackTests


class ThrottledOrigin(Origin):
    def do_GET(self):
        server = self.server
        if self.path.startswith("/background-"):
            with server.activity_lock:
                server.background += 1
        with server.activity_lock:
            server.active += 1
            server.peak_active = max(server.peak_active, server.active)
        try:
            if self.path.startswith("/background-"):
                self.stream_background()
            else:
                self.reply()
        finally:
            with server.activity_lock:
                server.active -= 1
                if self.path.startswith("/background-"):
                    server.background -= 1

    def stream_background(self):
        payload = self.server.background_payload
        self.send_response(200)
        self.send_header("Content-Type", "video/mp4")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            for start in range(0, len(payload), 65536):
                self.wfile.write(payload[start:start + 65536])
                time.sleep(0.012)
        except (BrokenPipeError, ConnectionResetError):
            pass


class BenchmarkFixture(PlaybackTests):
    origin_handler = ThrottledOrigin


def measure(clip, loaded):
    with_fixture = BenchmarkFixture()
    try:
        with_fixture.setUp()
        fixture = with_fixture
        origin = fixture.origin
        origin.mp4 = clip
        origin.background_payload = b"x" * (24 * 1024 * 1024)
        origin.activity_lock = threading.Lock()
        origin.active = origin.background = origin.peak_active = 0
        fixture.start(capacity=128 * 1024 * 1024, log_level="debug")
        base = f"http://127.0.0.1:{fixture.port}"
        if loaded:
            for index in range(2):
                address = fixture.source + f"/background-{index}.mp4"
                status, _, _ = fixture.get(method="POST", proxy_path="/v1/offline/" + address)
                if status != 202:
                    raise RuntimeError(f"offline task returned {status}")
            deadline = time.monotonic() + 5
            while origin.background != 2:
                if time.monotonic() >= deadline:
                    raise RuntimeError("two origin downloads did not start")
                time.sleep(0.01)

        peaks = {"proxy_rss_kib": 0, "cache_disk_bytes": 0}
        stop = threading.Event()

        def sample():
            while not stop.is_set():
                rss = subprocess.check_output(
                    ["ps", "-o", "rss=", "-p", str(fixture.proxy.pid)], text=True
                )
                disk = 0
                for path in fixture.cache.rglob("*"):
                    try:
                        if path.is_file():
                            disk += path.stat().st_size
                    except FileNotFoundError:
                        pass
                peaks["proxy_rss_kib"] = max(peaks["proxy_rss_kib"], int(rss))
                peaks["cache_disk_bytes"] = max(peaks["cache_disk_bytes"], disk)
                stop.wait(0.025)

        sampler = threading.Thread(target=sample)
        sampler.start()
        try:
            times = {}
            for metric, start in [("cold_first_frame_ms", "0"), ("cold_seek_first_frame_ms", "9")]:
                # Use independent source keys so both measurements start without cached video.
                url = f"{base}/v1/{fixture.source}/playable.mp4?sig=a%2Fb&z=1"
                if start != "0":
                    asset = "/seek.mp4"
                    origin.resources[asset] = ("video/mp4", clip)
                    url = f"{base}/v1/{fixture.source}{asset}"
                began = time.monotonic()
                output = subprocess.check_output(
                    [FFMPEG, "-v", "error", "-ss", start, "-short_seek_size", "0",
                     "-i", url, "-frames:v", "1", "-f", "framehash", "-"],
                    timeout=20, text=True,
                )
                if not any(row.startswith("0,") for row in output.splitlines()):
                    raise RuntimeError("FFmpeg did not decode a frame")
                times[metric] = round((time.monotonic() - began) * 1000, 2)
            if loaded and origin.background != 2:
                raise RuntimeError("background downloads ended before decoding measurements")
        finally:
            stop.set()
            sampler.join()
        return {**times, **peaks, "origin_peak_gets": origin.peak_active}
    except Exception as error:
        if with_fixture.proxy is not None:
            error.add_note(with_fixture.proxy_log.read_text()[-6000:])
        raise
    finally:
        with_fixture.doCleanups()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=5)
    args = parser.parse_args()
    if args.samples < 1 or FFMPEG is None:
        parser.error("requires FFmpeg and a positive sample count")
    with tempfile.TemporaryDirectory() as folder:
        path = Path(folder) / "fixture.mp4"
        subprocess.run([FFMPEG, "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
                        "-t", "12", "-c:v", "mpeg4", "-g", "12", "-q:v", "3",
                        "-movflags", "+faststart", str(path)], check=True, timeout=20)
        clip = path.read_bytes()
    results = {"idle": [], "two_offline_tasks": []}
    for index in range(args.samples):
        order = [False, True] if index % 2 == 0 else [True, False]
        for loaded in order:
            results["two_offline_tasks" if loaded else "idle"].append(measure(clip, loaded))
    for mode, samples in results.items():
        medians = {metric: statistics.median(sample[metric] for sample in samples) for metric in samples[0]}
        print(json.dumps({"mode": mode, "median": medians, "samples": samples}))


if __name__ == "__main__":
    main()
