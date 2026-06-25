# External Connector Sidecars

This directory contains production-facing sidecars that speak Kheish's
`external` connector protocol.

Design rules:

- `telegram`, `slack`, and plain `http` stay native in the daemon.
- Hermes-only platforms are added here as sidecars, not new daemon connector kinds.
- The daemon remains the authority for session binding, idempotence, runs, and delivery state.
- Child-process connectors are expected to receive these reserved env vars from the daemon:
  - `KHEISH_EXTERNAL_CONNECTOR_NAME`
  - `KHEISH_EXTERNAL_CONNECTOR_BASE_URL`
  - `KHEISH_EXTERNAL_CONNECTOR_DAEMON_BASE_URL`
  - `KHEISH_EXTERNAL_CONNECTOR_SHARED_TOKEN`
  - `KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN`
  - `KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON`

Available sidecars:

- `discord`
- `matrix`
- `email`
- `sms`
- `signal`
- `whatsapp`
- `webhook`

Notes:

- `signal` and `whatsapp` are intentionally marked `experimental` in the manifest.
- `matrix` is best-effort today: unencrypted rooms only, no E2E crypto, and long-poll `/sync`.
- Child-process sidecars can fetch secret-backed platform credentials from the daemon at startup via `credential_slots`; platform secrets do not need to live in the inherited process environment.
- Sidecar runtime endpoints still report `protocol_version = 1`; ingress submissions now default to protocol `2`.
- The shared helper in `common.py` supports both `submit_event(...)` and `submit_events_batch(...)`.

Run one sidecar manually:

```bash
python3 connectors/python/run_connector.py discord
```

Common test-only env vars used by daemon E2E tests:

- `KHEISH_EXTERNAL_CONNECTOR_ENABLE_TEST_API=true`
- `KHEISH_EXTERNAL_CONNECTOR_TEST_MODE=true`
- `KHEISH_EXTERNAL_CONNECTOR_DELIVERY_CAPTURE_PATH=/tmp/captures.jsonl`

With both `ENABLE_TEST_API` and `TEST_MODE`, the sidecar exposes
`POST /__test/inject` on its own base URL. This stays disabled by default and is
only meant for controlled loopback E2E scenarios with a shared token.

With `DELIVERY_CAPTURE_PATH`, `/deliver` records the daemon payload and returns
`committed` without calling the platform-specific send path. This is useful for
real-daemon protocol coverage, but it is not a substitute for provider-backed
live egress validation.
