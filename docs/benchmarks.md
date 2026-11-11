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

## Quiet microphones and automatic gain (`src/agc.rs`)

The maintainer's ArctisX mic peaks at about 0.0085 on room noise. The same JFK clip,
attenuated:

| Input peak | Without AGC | With AGC (target peak 0.5, max +20 dB) |
|---|---|---|
| 0.045 (−20 dB) | all 17 words, avg lag 0.82 s | all 17 words, avg lag 0.79 s |
| 0.013 (−30 dB) | **first 4 words lost** (VAD missed the onset), avg lag 1.88 s | all 17 words, avg lag 0.84 s |
| room noise only (3 s, real mic) | 0 decodes | 0 decodes (no false triggers) |

Normal-level speech is unchanged with AGC on.

## Model comparison (`examples/simulate --no-ref --agc --gap 0`)

Input: eight synthesized sentences back to back, 43.5 s of continuous speech, release build,
4 threads (Light: 2).

Moonshine tiny and Light were dropped from the catalogue afterwards (D-021). Their rows stay as
a record.

| Model | Load | Peak RAM | CPU (decode / audio) | Avg lag | Notes |
|---|---|---|---|---|---|
| Parakeet v3 | 1.4 s | 1.1 GB | 26 % | 1.04 s | most accurate |
| Moonshine base v2 | 0.46 s | 355 MB | 22 % | ~1 s* | good; no timestamps |
| Moonshine tiny v2 | 0.25 s | 170 MB | 14 % | ~1 s* | more mishearings |
| Light (Zipformer 2023-06-26 + punct) | 1.2 s | 176 MB | **5 %** | 0.94 s | digits spelled out, occasional casing slips |

\* Sequence mode reports no audio-time lag. Words commit after two agreeing runs, with the
edge word held back.

- Light engine **append-only check:** 0 violations over the corpus (the TODO item).
- Rejected: the 20M Zipformer drops the first words of utterances, even when fed directly to
  sherpa (`examples/light_probe`, since removed). The Kroko 2025 streaming model gives native punctuation
  and casing, but its license is unclear (D-008).
- With an idle unload of 10 min, idle RAM drops back to the base daemon.
