---
description: Generate spectrograms and audio feature visualizations (mel, chroma,
  MFCC, tempogram, etc.) from audio files via CLI. Useful for audio analysis, music
  production debugging, and visual documentation.
version: 1.0.0
author: community
license: MIT
metadata:
  catalog:
    tags:
    - Audio
    - Visualization
    - Spectrogram
    - Music
    - Analysis
    homepage: https://github.com/steipete/songsee
prerequisites:
  commands:
  - songsee
---

## Kheish Compatibility

This skill is repo-local and stays inactive until explicitly activated.

When the original instructions refer to legacy tool names, use these Kheish mappings:

- `terminal` => `bash`
- `web_extract` => `web_fetch`, plus `web_search` when discovery is needed
- `search_files` => `grep_search` and `glob_search`
- `browser_*` tools require a browser-capable surfaced tool or MCP; if none is available, use the closest available surface and say so explicitly

When the instructions mention local helper files, resolve them from `${KHEISH_SKILL_DIR}`.

# songsee

Generate spectrograms and multi-panel audio feature visualizations from audio files.

## Prerequisites

Requires [Go](https://go.dev/doc/install):
```bash
go install github.com/steipete/songsee/cmd/songsee@latest
```

Optional: `ffmpeg` for formats beyond WAV/MP3.

## Quick Start

```bash
# Basic spectrogram
songsee track.mp3

# Save to specific file
songsee track.mp3 -o spectrogram.png

# Multi-panel visualization grid
songsee track.mp3 --viz spectrogram,mel,chroma,hpss,selfsim,loudness,tempogram,mfcc,flux

# Time slice (start at 12.5s, 8s duration)
songsee track.mp3 --start 12.5 --duration 8 -o slice.jpg

# From stdin
cat track.mp3 | songsee - --format png -o out.png
```

## Visualization Types

Use `--viz` with comma-separated values:

| Type | Description |
|------|-------------|
| `spectrogram` | Standard frequency spectrogram |
| `mel` | Mel-scaled spectrogram |
| `chroma` | Pitch class distribution |
| `hpss` | Harmonic/percussive separation |
| `selfsim` | Self-similarity matrix |
| `loudness` | Loudness over time |
| `tempogram` | Tempo estimation |
| `mfcc` | Mel-frequency cepstral coefficients |
| `flux` | Spectral flux (onset detection) |

Multiple `--viz` types render as a grid in a single image.

## Common Flags

| Flag | Description |
|------|-------------|
| `--viz` | Visualization types (comma-separated) |
| `--style` | Color palette: `classic`, `magma`, `inferno`, `viridis`, `gray` |
| `--width` / `--height` | Output image dimensions |
| `--window` / `--hop` | FFT window and hop size |
| `--min-freq` / `--max-freq` | Frequency range filter |
| `--start` / `--duration` | Time slice of the audio |
| `--format` | Output format: `jpg` or `png` |
| `-o` | Output file path |

## Notes

- WAV and MP3 are decoded natively; other formats require `ffmpeg`
- Output images can be inspected with `vision_analyze` for automated audio analysis
- Useful for comparing audio outputs, debugging synthesis, or documenting audio processing pipelines
