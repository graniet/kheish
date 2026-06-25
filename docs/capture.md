# Capture and Observations

Kheish treats system capture as a generic observation problem rather than a provider-specific media feature.

- the daemon owns durable `observation sources` with retention, sensitivity, and materialization policy
- the daemon can provision host-local capture agents with OS profiles, expiring token leases, heartbeat deadlines, and one-command revocation
- external producers upload `observations` into those sources through a stable ingest API
- the daemon stores the raw payload plus optional canonical text, caller metadata, `stream_id`, and `seq_no`
- later, one operator or automation flow can materialize those observations into a normal session run

## Source kinds and media types

The current daemon source kinds:

- `screen_snapshot`
- `webcam_snapshot`
- `microphone_segment`

Accepted media types:

- `screen_snapshot`: `image/png`, `image/jpeg`
- `webcam_snapshot`: `image/png`, `image/jpeg`
- `microphone_segment`: `audio/wav`, `audio/webm`

This keeps capture provider-neutral and host-neutral. The daemon does not need to know whether an observation came from a host-local capture process, a connector, a browser extension, or another system entirely.

## Reference host-local runtime: `kheish-capture`

The current host-local reference runtime is `kheish-capture`, developed as a sibling repository. It currently implements:

- fixture-based `image/png`, `image/jpeg`, `audio/wav`, and `audio/webm` uploads
- one live screen driver that emits `image/png` or `image/jpeg`
- one live webcam driver that emits `image/png` or `image/jpeg`
- one live microphone driver that emits `audio/wav`
- one live macOS system/output audio driver that emits `audio/wav`
- macOS onboarding commands for device discovery, config generation, diagnostics, and LaunchAgent setup

Its real-daemon test coverage validates the daemon contract through fixture and synthetic-driver flows, plus conditional live screen and webcam CLI paths when the current environment exposes the required backends and devices. Host-device behavior can still vary across operating systems and available capture backends.

## Capture-group correlation

For correlated multi-artifact capture, the daemon supports source/stream filters and capture-group materialization:

- keep stable daemon sources per host, such as screen, webcam, and call-audio
- use distinct `stream_id` values for each uploaded leg
- attach a shared `metadata.capture_group_id`
- materialize with `observations materialize --capture-group-id ...`

The default materialization context renders source ids, stream ids, sequence numbers, selected roles, capture-group metadata, and timing so agents can reason over related screenshot, webcam, and audio observations in one run.

## Capture-agent provisioning

The daemon exposes admin-only capture-agent provisioning. It creates or rotates the observation sources for a fleet and returns per-host runtime configs with source-scoped upload tokens only; capture hosts do not need daemon admin tokens at runtime.

## Audio derivation

Audio fits the generic derivation model:

- raw `audio/wav` and `audio/webm` assets remain the source of truth
- `canonical_text` can be derived daemon-side from audio assets and audio observations when a transcription backend is configured
- inline audio attachments submitted through normal session input use the same canonical-text path before the daemon freezes the rendered prompt content
- the built-in transcription backends are OpenAI and OpenRouter; the daemon capability is provider-neutral
