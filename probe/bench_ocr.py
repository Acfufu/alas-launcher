#!/usr/bin/env python3
"""OCR microbench replicating ALAS payload module/ocr/al_ocr.py exactly.

Engines: cpu (CPUExecutionProvider, as deployed today) or coreml
(CoreMLExecutionProvider -> ANE on Apple Silicon). Modes: mixed feeds raw
crop widths (dynamic W, what ALAS does today); bucket pads W to multiples
of 64 (the proposed ANE-friendly shape policy).

Writes a JSON summary so the phase driver can record throughput per phase.
"""
import argparse
import json
import os
import sys
import time

import cv2
import numpy as np
import onnxruntime

cv2.setNumThreads(2)  # payload parity (al_ocr.py)

IMG_HEIGHT = 32
SEQ_LEN_CMPR_RATIO = 4

MODEL = "/Users/acfufu/Codehub/alas-payload/bin/cnocr_models/azur_lane/azur_lane.onnx"
LABEL = "/Users/acfufu/Codehub/alas-payload/bin/cnocr_models/azur_lane/label_cn.txt"
FIXTURE = "/Users/acfufu/Codehub/alas-launcher/probe/out/fixture.png"

# Rects (x1, y1, x2, y2) on the 720h x 1280w fixture; chosen to give a spread
# of native widths after h32 resize, like real ALAS OCR regions do.
CROPS = [
    (0, 0, 1280, 64),      # -> W 640
    (900, 90, 1280, 210),  # -> W ~101
    (480, 560, 800, 640),  # -> W 128
    (40, 200, 240, 500),   # -> W ~21
]

COREML_OPTS_CANDIDATES = [
    {"ModelFormat": "MLProgram", "MLComputeUnits": "ALL"},
    {"MLComputeUnits": "ALL"},
    None,  # plain provider string
]


def read_alphabet():
    alphabet = [None]
    with open(LABEL, encoding="utf-8") as fp:
        for line in fp:
            alphabet.append(line.rstrip("\n"))
    return alphabet


def make_session(engine, model=MODEL):
    so = onnxruntime.SessionOptions()
    so.intra_op_num_threads = 2
    if engine == "cpu":
        return onnxruntime.InferenceSession(model, so, providers=["CPUExecutionProvider"])
    last_err = None
    for opts in COREML_OPTS_CANDIDATES:
        try:
            if opts is None:
                provs = ["CoreMLExecutionProvider", "CPUExecutionProvider"]
            else:
                provs = [("CoreMLExecutionProvider", opts), "CPUExecutionProvider"]
            s = onnxruntime.InferenceSession(model, so, providers=provs)
            if s.get_providers()[0] == "CoreMLExecutionProvider":
                return s
            last_err = RuntimeError(f"CoreML not first provider: {s.get_providers()}")
        except Exception as e:  # noqa: BLE001
            last_err = e
    raise RuntimeError(f"Could not create CoreML session: {last_err}")


def preprocess(img_bgr):
    """Replicate AlOcr._preprocess_img_array for a BGR crop -> (1,32,W) f32."""
    img = np.array(cv2.cvtColor(img_bgr, cv2.COLOR_BGR2RGB))
    from PIL import Image
    img = np.array(Image.fromarray(img).convert("L"))
    new_w = round(IMG_HEIGHT / img.shape[0] * img.shape[1])
    img = cv2.resize(img, (new_w, IMG_HEIGHT))
    return np.expand_dims(img, 0).astype("float32") / 255.0


def pad_to(arr, width):
    if arr.shape[2] >= width:
        return arr
    return np.pad(arr, ((0, 0), (0, 0), (0, width - arr.shape[2])))


def decode(prob, img_width, max_img_width, alphabet):
    """Replicate AlOcr._gen_line_pred_chars + _ctc_label. prob: [T, classes]."""
    class_ids = np.argmax(prob, axis=-1)
    class_ids = class_ids * (np.max(prob, axis=-1) > 0.5)
    if img_width < max_img_width:
        end_idx = img_width // SEQ_LEN_CMPR_RATIO
        if end_idx < len(class_ids):
            class_ids[end_idx:] = 0
    p1 = [0, *class_ids.tolist()]
    pred = []
    for i in range(len(class_ids)):
        c1, c2 = p1[i], p1[i + 1]
        if c2 == 0 or c2 == c1:
            continue
        pred.append(c2)
    return "".join(alphabet[p] if alphabet[p] != "<space>" else " " for p in pred)


def run_batch(sess, arrs):
    """ALAS pads batch to max width; returns list of decoded strings."""
    widths = [a.shape[2] for a in arrs]
    max_w = max(widths)
    batch = np.ascontiguousarray(np.array([pad_to(a, max_w) for a in arrs], dtype="float32"))
    t0 = time.perf_counter()
    prob = sess.run(["probs"], {"data": batch})[0]
    dt = time.perf_counter() - t0
    prob = np.reshape(prob, (-1, len(arrs), prob.shape[1]))
    texts = [decode(prob[:, i, :], w, max_w, ALPHABET) for i, w in enumerate(widths)]
    return texts, dt


ALPHABET = read_alphabet()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", choices=["cpu", "coreml"], required=True)
    ap.add_argument("--mode", choices=["mixed", "bucket", "fixed640"], required=True)
    ap.add_argument("--model", default=MODEL,
                    help="ONNX path; pass a static-shape re-export to probe ANE")
    ap.add_argument("--until-epoch", type=float, required=True)
    ap.add_argument("--json-out", default=None)
    args = ap.parse_args()

    img = cv2.imread(FIXTURE)
    crops = [img[y1:y2, x1:x2] for (x1, y1, x2, y2) in CROPS]
    if args.mode == "bucket":
        arrs = [pad_to(preprocess(c), -(-preprocess(c).shape[2] // 64) * 64) for c in crops]
    elif args.mode == "fixed640":
        arrs = [pad_to(preprocess(c), 640) for c in crops]
    else:
        arrs = [preprocess(c) for c in crops]

    t_load0 = time.perf_counter()
    sess = make_session(args.engine, args.model)
    load_s = time.perf_counter() - t_load0

    # Warmup: covers every shape once so CoreML compile / first-touch cost
    # lands before the measured loop.
    for a in arrs:
        run_batch(sess, [a])

    n = 0
    infer_ms = []
    e2e_ms = []
    while time.time() < args.until_epoch:
        a = arrs[n % len(arrs)]
        t0 = time.perf_counter()
        _, dt = run_batch(sess, [a])
        e2e_ms.append((time.perf_counter() - t0) * 1000)
        infer_ms.append(dt * 1000)
        n += 1

    # Parity: cpu vs this engine, same crops.
    if args.engine == "cpu":
        parity = None
        ref_texts = None
    else:
        cpu_sess = make_session("cpu")
        cpu_texts = [run_batch(cpu_sess, [a])[0][0] for a in arrs]
        ml_texts = [run_batch(sess, [a])[0][0] for a in arrs]
        ref_texts = ml_texts
        parity = all(c == m for c, m in zip(cpu_texts, ml_texts))

    out = {
        "engine": args.engine,
        "mode": args.mode,
        "providers_active": sess.get_providers(),
        "load_s": round(load_s, 3),
        "iters": n,
        "infer_ms_avg": round(float(np.mean(infer_ms)), 3) if infer_ms else None,
        "infer_ms_p50": round(float(np.percentile(infer_ms, 50)), 3) if infer_ms else None,
        "e2e_ms_avg": round(float(np.mean(e2e_ms)), 3) if e2e_ms else None,
        "shapes": [list(a.shape) for a in arrs],
        "parity_vs_cpu": parity,
        "sample_texts": ref_texts,
    }
    # recompute ops/s honestly: measured window = n * avg e2e (loop is back-to-back)
    if e2e_ms:
        out["ops_per_s"] = round(1000.0 / (sum(e2e_ms) / len(e2e_ms)), 3)
    else:
        out["ops_per_s"] = None
    txt = json.dumps(out, ensure_ascii=False)
    print(txt)
    if args.json_out:
        with open(args.json_out, "w") as fp:
            fp.write(txt)


if __name__ == "__main__":
    sys.exit(main())
