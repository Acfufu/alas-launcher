#!/usr/bin/env python3
"""Screenshot-path microbench against emulator-5554.

mode png : adb exec-out screencap -p   (payload Adb.screenshot_adb, deployed default)
mode raw : adb exec-out screencap      (payload screenshot_adb_nc path; skips
                                          on-device PNG encode + host PNG decode)
Loops until --until-epoch, prints JSON summary.
"""
import argparse
import json
import shutil
import statistics
import subprocess
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serial", default="emulator-5554")
    ap.add_argument("--mode", choices=["png", "raw"], required=True)
    ap.add_argument("--until-epoch", type=float, required=True)
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()

    adb = shutil.which("adb") or "/Users/acfufu/Library/Android/sdk/platform-tools/adb"
    cmd = [adb, "-s", args.serial, "exec-out", "screencap"]
    if args.mode == "png":
        cmd.append("-p")

    lat = []
    sizes = []
    while time.time() < args.until_epoch:
        t0 = time.perf_counter()
        r = subprocess.run(cmd, capture_output=True)
        dt = (time.perf_counter() - t0) * 1000
        if r.returncode == 0 and len(r.stdout) > 1000:
            lat.append(dt)
            sizes.append(len(r.stdout))
        else:
            time.sleep(0.05)

    out = {
        "bench": "screencap",
        "mode": args.mode,
        "frames": len(lat),
        "fps_sustained": round(1000.0 / (sum(lat) / len(lat)), 3) if lat else None,
        "ms_avg": round(statistics.mean(lat), 1) if lat else None,
        "ms_p50": round(statistics.median(lat), 1) if lat else None,
        "ms_max": round(max(lat), 1) if lat else None,
        "bytes_avg": round(statistics.mean(sizes)) if sizes else None,
    }
    txt = json.dumps(out)
    print(txt)
    if args.json_out:
        with open(args.json_out, "w") as fp:
            fp.write(txt)


if __name__ == "__main__":
    main()
