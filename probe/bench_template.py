#!/usr/bin/env python3
"""Template-matching microbench (cv2.matchTemplate, payload-style).

Uses two template sizes cut from the fixture, searched over the full
720x1280 frame with TM_CCOEFF_NORMED — the workload ALAS runs for every
template asset check. cv2 thread count matches payload (2).
"""
import argparse
import json
import sys
import time

import cv2
import numpy as np

cv2.setNumThreads(2)

FIXTURE = "/Users/acfufu/Codehub/alas-launcher/probe/out/fixture.png"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--until-epoch", type=float, required=True)
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()

    img = cv2.imread(FIXTURE)
    t64 = img[300:364, 600:664]
    t128 = img[600:728, 900:1028]
    jobs = [(img, t64), (img, t128)]

    n = 0
    lat = []
    t_start = time.time()
    while time.time() < args.until_epoch:
        big, tmpl = jobs[n % 2]
        t0 = time.perf_counter()
        cv2.matchTemplate(big, tmpl, cv2.TM_CCOEFF_NORMED)
        lat.append((time.perf_counter() - t0) * 1000)
        n += 1

    out = {
        "bench": "template",
        "iters": n,
        "ms_avg": round(float(np.mean(lat)), 2) if lat else None,
        "ms_p50": round(float(np.percentile(lat, 50)), 2) if lat else None,
        "ms_max": round(float(np.max(lat)), 2) if lat else None,
        "match_s": round(n / max(1e-9, time.time() - t_start), 2) if lat else None,
    }
    txt = json.dumps(out)
    print(txt)
    if args.json_out:
        with open(args.json_out, "w") as fp:
            fp.write(txt)


if __name__ == "__main__":
    sys.exit(main())
