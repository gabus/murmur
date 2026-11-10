# Benchmarks

Machine: Ryzen 7 7700X (8C/16T), 62 GB RAM, Fedora 44. CPU inference only.
sherpa-onnx 2.0.1 (static prebuilt), Parakeet TDT 0.6B v3 int8, Silero VAD.
Release builds run in the `murmur-dev` toolbox.

## Parakeet decode time per buffer length (`examples/asr_probe`, 4 threads)

| Audio in buffer | Decode | RTF |
|---|---|---|
| 0.96 s | 59 ms | 0.06 |
| 1.92 s | 82 ms | 0.04 |
| 2.88 s | 113 ms | 0.04 |
| 3.85 s | 138 ms | 0.04 |

Model load: 1.5 s (warm page cache) to 2.9 s (cold), including Silero.

## Re-check engine, streamed (`examples/simulate`, 20 ms chunks)

Input: `en.wav + de.wav + en.wav` from the model's `test_wavs`, 1.5 s silence between,
15.4 s total. Engine defaults: re-run every 0.5 s, `min_age` 0.5 s, VAD end-of-phrase
0.8 s.

| Metric | Value |
|---|---|
| Lag, word end → committed (audio time) | avg **0.79 s**, max 1.16 s (+ ~0.1 s decode wall time) |
| Decode wall time / audio time | **16 %** (28 decodes, 4 threads) |
| Language switches en→de→en | no flip-flopping; each phrase correct |
| Difference vs. whole-file decode | `you.` / `Ask` instead of `you,` / `ask` at the speaker's pause, committed before the next phrase's context existed. This is inherent to the ~1 s commit delay. |

Answers the TODO open question on re-check CPU cost: Parakeet re-run every 0.5 s is
affordable on this CPU. Weaker CPUs still need measuring (Stage 3 benchmarks).
