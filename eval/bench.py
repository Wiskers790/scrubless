"""Benchmarks for Scrubless, run against the live app's library and engine (dev build).

  MSR-VTT 1k-A   text -> video retrieval (R@1/5/10, median rank), several frame aggregations
  FLEURS dev     transcription accuracy (WER / CER), exact-phrase search through the app,
                 cross-language meaning search (English sentence -> es/ja/hi recording)

  uv run --no-project --with numpy python eval/bench.py [msrvtt|fleurs|all]

Needs the dev build running with SCRUBLESS_DEV_BRIDGE=1430, the datasets added to its library
(MSR-VTT 1k-A test videos from friedrichor/MSR-VTT; FLEURS dev audio for en_us, es_419, ja_jp
and hi_in, one recording per sentence id saved as <lang>/<id>.wav, with fleurs_eval.json
{lang: {id: transcript}}), and SCRUBLESS_DB / SCRUBLESS_EVAL pointing at them.
"""
import json, os, random, re, sqlite3, subprocess, sys, unicodedata, urllib.request
from collections import defaultdict
from pathlib import Path

import numpy as np

# point these at your dev data dir and downloaded datasets (see the docstring)
DB = os.environ.get("SCRUBLESS_DB", "eval-data/library.db")
EVAL = Path(os.environ.get("SCRUBLESS_EVAL", "eval-data"))
BRIDGE = "http://127.0.0.1:1430/invoke"
OUT = Path(__file__).parent / "results.json"


def engine_port():
    for pid in subprocess.run(["pgrep", "-x", "llama-server"], capture_output=True, text=True).stdout.split():
        cmd = open(f"/proc/{pid}/cmdline").read().split("\0")
        if any(c.endswith("embeddinggemma-2-Q8_0.gguf") for c in cmd):
            return int(cmd[cmd.index("--port") + 1])
    raise SystemExit("app engine not running")


def embed(texts, prefix="task: search result | query: ", batch=16):
    port, out = engine_port(), []
    for i in range(0, len(texts), batch):
        body = json.dumps({"input": [prefix + t for t in texts[i:i + batch]]}).encode()
        r = urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}/v1/embeddings", data=body,
                                                          headers={"Content-Type": "application/json"}))
        out += [d["embedding"] for d in json.load(r)["data"]]
    v = np.array(out, dtype=np.float32)
    return v / np.linalg.norm(v, axis=1, keepdims=True)


def bridge(cmd, **args):
    r = urllib.request.urlopen(urllib.request.Request(f"{BRIDGE}/{cmd}", data=json.dumps(args).encode()))
    return json.load(r)


def items(folder, kind):
    """{file name: (vectors [n, 768], texts)} for items of `kind` under `folder`."""
    db = sqlite3.connect(DB)
    rows = db.execute("select f.path, i.vec, i.text, i.t0 from items i join files f on f.id=i.file_id "
                      "where i.kind=? and f.path like ? order by f.path, i.t0", (kind, f"{folder}/%")).fetchall()
    by = defaultdict(lambda: ([], []))
    for path, vec, text, _ in rows:
        v = np.frombuffer(vec, dtype=np.float16).astype(np.float32)
        by[Path(path).name][0].append(v / np.linalg.norm(v))
        by[Path(path).name][1].append(text)
    return {k: (np.stack(v), t) for k, (v, t) in by.items()}


def recall(ranks, n):
    r = np.array(ranks)
    return {"n": n, "R@1": round(float(np.mean(r <= 1)) * 100, 1), "R@5": round(float(np.mean(r <= 5)) * 100, 1),
            "R@10": round(float(np.mean(r <= 10)) * 100, 1), "MdR": float(np.median(r))}


# ---------------------------------------------------------------- MSR-VTT

def msrvtt():
    ann = json.load(open(EVAL / "msrvtt_test_1k.json"))
    vids = items(str(EVAL / "msrvtt"), "frame")
    ann = [a for a in ann if a["video"] in vids]
    names = [a["video"] for a in ann]
    Q = embed([a["caption"] for a in ann])
    agg = {
        "max (app)": lambda s: s.max(),
        "mean top-2": lambda s: np.sort(s)[-2:].mean(),
        "mean top-3": lambda s: np.sort(s)[-3:].mean(),
        "mean all": lambda s: s.mean(),
        "softmax-pool": lambda s: float(np.log(np.exp(s * 50).sum()) / 50),
    }
    mats = [vids[n][0] for n in names]
    res = {}
    for label, f in agg.items():
        S = np.array([[f(m @ q) for m in mats] for q in Q])  # [query, video]
        ranks = [int((S[i] > S[i, i]).sum()) + 1 for i in range(len(names))]
        res[label] = recall(ranks, len(names))
    frames = [len(m) for m in mats]
    res["frames_per_clip"] = {"mean": round(float(np.mean(frames)), 1), "max": int(max(frames))}
    # the app end to end (bridge search restricted to the folder) on a sample: rank of the first
    # result from the right clip, counting only distinct clips
    random.seed(0)
    sample = random.sample(range(len(ann)), min(150, len(ann)))
    app_ranks = []
    for i in sample:
        r = bridge("search", query=ann[i]["caption"], filters={"folders": [str(EVAL / "msrvtt")]})
        order = list(dict.fromkeys(Path(h["path"]).name for h in r["moments"]))
        app_ranks.append(order.index(names[i]) + 1 if names[i] in order else 999)
    res["app end-to-end (150 sample)"] = recall(app_ranks, len(sample))
    return res


# ---------------------------------------------------------------- FLEURS

def norm(s, lang):
    s = unicodedata.normalize("NFKC", s).lower()
    s = re.sub(r"[^\w\s]", " ", s)
    return s.split() if lang not in ("ja_jp",) else list(re.sub(r"\s+", "", s))


def edit_distance(a, b):
    d = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        prev, d[0] = d[0], i
        for j, y in enumerate(b, 1):
            prev, d[j] = d[j], min(d[j] + 1, d[j - 1] + 1, prev + (x != y))
    return d[-1]


def fleurs():
    meta = json.load(open(EVAL / "fleurs/fleurs_eval.json"))
    langs = ["en_us", "es_419", "ja_jp", "hi_in"]
    res = {"transcription": {}, "exact_phrase": {}, "cross_language": {}}
    speech = {l: items(str(EVAL / "fleurs" / l), "speech") for l in langs}
    for l in langs:
        errs, total, missing = 0, 0, 0
        for sid, ref in meta[l].items():
            hyp = " ".join(t for t in speech[l].get(f"{sid}.wav", (None, []))[1])
            if not hyp:
                missing += 1
            r, h = norm(ref, l), norm(hyp, l)
            errs += edit_distance(r, h)
            total += len(r)
        res["transcription"][l] = {"metric": "CER" if l == "ja_jp" else "WER", "error_%": round(100 * errs / total, 1),
                                   "files": len(meta[l]), "untranscribed": missing}
    # exact phrase: a 4-word (Japanese: 7-character) span of the *reference* text; the app must
    # return the right recording as an exact hit. Whisper's wording has to match the reference.
    random.seed(1)
    for l in langs:
        hit1, found, said1, n = 0, 0, 0, 0
        for sid, ref in list(meta[l].items())[:60]:
            toks = ref.split() if l != "ja_jp" else None
            if l == "ja_jp":
                clean = re.sub(r"[、。「」\s]", "", ref)
                if len(clean) < 10:
                    continue
                s = random.randint(0, len(clean) - 8)
                q = clean[s:s + 7]
            else:
                if len(toks) < 6:
                    continue
                s = random.randint(0, len(toks) - 5)
                # drop punctuation only (\w would also drop Devanagari vowel signs)
                q = "".join(c for c in " ".join(toks[s:s + 4]) if not unicodedata.category(c).startswith("P"))
            r = bridge("search", query=q, filters={"folders": [str(EVAL / "fleurs" / l)]})
            exact = [Path(h["path"]).stem for h in r["said"] if h["exact"]]
            n += 1
            found += sid in exact
            hit1 += bool(exact) and exact[0] == sid
            # what the user sees: the first "Said" result, exact or by meaning
            said1 += bool(r["said"]) and Path(r["said"][0]["path"]).stem == sid
        res["exact_phrase"][l] = {"queries": n, "exact_found_%": round(100 * found / n, 1), "exact_top1_%": round(100 * hit1 / n, 1),
                                  "said_top1_%": round(100 * said1 / n, 1)}
    # cross-language: the English sentence finds the same sentence spoken in another language
    ids = list(meta["en_us"])
    Q = embed([meta["en_us"][i] for i in ids])
    for l in ["es_419", "ja_jp", "hi_in", "en_us"]:
        files = [f"{i}.wav" for i in ids if f"{i}.wav" in speech[l]]
        mats = [speech[l][f][0] for f in files]
        ranks = []
        for qi, sid in enumerate(ids):
            if f"{sid}.wav" not in speech[l]:
                continue
            s = np.array([float((m @ Q[qi]).max()) for m in mats])
            ranks.append(int((s > s[files.index(f"{sid}.wav")]).sum()) + 1)
        res["cross_language"][f"en query -> {l} speech"] = recall(ranks, len(files))
    return res


if __name__ == "__main__":
    what = sys.argv[1] if len(sys.argv) > 1 else "all"
    out = json.loads(OUT.read_text()) if OUT.exists() else {}
    if what in ("msrvtt", "all"):
        out["msrvtt"] = msrvtt()
    if what in ("fleurs", "all"):
        out["fleurs"] = fleurs()
    OUT.write_text(json.dumps(out, indent=1, ensure_ascii=False))
    print(json.dumps(out, indent=1, ensure_ascii=False))
