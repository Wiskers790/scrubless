# Scrubless benchmark results

Run 2026-10-07 on an RTX 3050 (8 GB) with the app's own index and engines: EmbeddingGemma 2 Q8_0
through llama.cpp b11461 (Vulkan), and Whisper large-v3-turbo q5_0 through whisper.cpp 1.9.5
(Vulkan) with Silero VAD. Reproduce with `eval/bench.py`. Raw numbers are in `results.json`.

## Visual search: MSR-VTT 1k-A (1,000 clips, one human caption each, zero-shot)

| Ranking | R@1 | R@5 | R@10 | Median rank |
|---|---|---|---|---|
| Best frame per clip (what the app does) | **39.4** | **63.5** | **73.4** | 3 |
| Mean of top-2 frames | 36.3 | 61.5 | 73.0 | 3 |
| Mean of all frames | 34.1 | 59.3 | 70.8 | 3 |
| Soft max-pool | 39.0 | 64.0 | 73.9 | 2 |
| App end to end, 150-query sample (search through the UI backend with a folder filter) | 44.7 | 64.7 | 73.3 | 2 |

The app samples ~3.2 frames per clip (one per shot). For context, from published work: zero-shot
CLIP is around 30 R@1, and CLIP4Clip fine-tuned on MSR-VTT is around 44. A 740M multimodal
embedding model running locally is in that range without any training on this data.

## Speech: FLEURS dev, 116 sentences recorded in each of 4 languages

**Transcription** (versus human references; Japanese uses character error rate):

| | English | Spanish | Japanese | Hindi |
|---|---|---|---|---|
| Error rate | 5.8% WER | 7.2% WER | 4.0% CER | 33.3% WER |

Hindi is weak on auto-detect: Whisper writes some Hindi speech in Urdu script and spells noisily.
**Setting the speech language to Hindi (Settings → Speech) cuts Hindi WER from 33.3% to 19.3%**,
removes all Urdu-script output, and raises "first Said result is right" from 65% to 72%.

**Finding a recording from a remembered 4-word phrase** (7 characters for Japanese), 60 queries
per language:

| | English | Spanish | Japanese | Hindi |
|---|---|---|---|---|
| Exact-phrase match found | 63% | 65% | 72% | 27% |
| **First "Said" result is the right recording** (exact + by meaning) | **98%** | **93%** | **82%** | **65%** |

Exact matching is brittle by nature: one misheard word breaks it. Meaning-based search covers the
rest.

**Cross-language:** an English sentence as the query, looking for the same sentence spoken in
another language:

| Query → speech | R@1 | R@5 | R@10 |
|---|---|---|---|
| English → Spanish | 99.1 | 100 | 100 |
| English → Japanese | 99.1 | 100 | 100 |
| English → Hindi | 94.0 | 96.6 | 100 |

## Sound (ESC-50, 2,000 clips, 50 classes)

- Sound → similar sounds: P@10 0.65 (random 0.025)
- Typed text → sound: P@10 0.33, uneven by class, and no usable "is it there?" signal. It ships
  labelled beta.

## Bugs found by these tests (fixed)

1. Files indexed after app start were invisible to search until restart. App end-to-end R@10
   went from 58% to 73%.
2. The VAD clipped the first and last words of recordings. English WER went from 7.9% to 5.8%.
3. Punctuation and whisper's Devanagari spacing broke exact matching (normalized index).
4. Quiet recordings were never transcribed: transcription was tied to the sound-search loudness
   threshold.
5. Engine processes were killed when their spawning thread ended (Linux PDEATHSIG), and orphaned
   engines survived app restarts.
