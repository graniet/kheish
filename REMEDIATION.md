# Kheish remediation plan

**Date:** 2026-06-30
**Basis:** Functional audit (overall 8/10) followed by a per-weakness deep review — each weakness re-verified against the actual code (file:line) by a dedicated reviewer. All findings **CONFIRMED** except auth Gap 3 (partially refuted — the path is already fail-closed). Prioritized by **severity × reachability ÷ effort**, not by audit order.

Legend: `[x]` done · `[~]` partially done · `[ ]` not started. Effort: XS/S/M/L.

---

## Priority summary

| # | Item | Severity | Effort | Status |
|---|------|----------|--------|--------|
| **P0.1** | Fix 2 red tests on `main` | Baseline | XS | `[x]` applied + verified |
| **P0.2** | Add CI (fmt + build + lib-test; clippy advisory) | Baseline | S | `[x]` done + validated |
| **P1.1** | Anthropic request-shape 400 on current models | High (core path broken) | S–M | `[x]` done + 6 unit tests (2026-07-03) |
| **P1.2** | OutputHost empty-target fan-out → cross-transport leak | High | S–M | `[ ]` designed |
| **P1.3** | Lock-poison cascade (79 sites) → abort | High (availability) | M | `[x]` done — full workspace swap to parking_lot (~700 sites, 2026-07-03) |
| **P1.4** | Auth-store: no AAD + silently accepts plaintext | High (security) | M | `[ ]` designed |
| **P1.5** | Audit-key co-located: honesty + relocate flag | High (security) | S then M | `[~]` S part done (warning + docs); relocate flag (M) pending |
| **P2.1** | Mid-run durability **Phase 0** (incremental journal, torn-line, O(Δ)) | High (the thesis gap) | S–M | `[x]` done + 8 tests (2026-07-03) |
| **P2.2** | Google provider non-streaming → >90s timeout | Medium (correctness) | M | `[ ]` designed |
| **P2.3** | Rate limiter ~2× burst → token bucket | Medium | S | `[x]` done + 7 unit tests |
| **P2.4** | `observation.rs` zero unit tests (security-critical) | Medium | M | `[~]` rate-limiter portion seeded by P2.3 |
| **P2.5** | Companion image-route bug (generate_image/audio) | Medium | XS | `[x]` done + e2e test (teeth-proven) |
| **P3.1** | Mid-run durability **Phase 1** (auto-resume) | High but risky/large | L | `[ ]` designed |
| **P3.2** | `never_loop` dead-loop bug at `assets.rs:396` | Medium (latent) | XS | `[x]` done |
| **P3.3** | Auth-store KDF (argon2id, passphrase keys only) | Medium (security) | M | `[ ]` designed |
| **P3.4** | Provider dedup + **Retry-After HTTP-date** bonus bug | Low+ | M | `[ ]` designed |
| **P3.5** | Clippy debt paydown → blocking gate | Hygiene | L | `[ ]` scoped |
| **P3.6** | Auth-off loopback warning + guard | Low | S | `[x]` warning done + gap-filling test |

---

## Implementation log — 2026-06-30 (S + XS tasks)

All **S and XS** tasks were implemented, adversarially reviewed (two independent refute-first reviewers), and e2e/unit tested. The full CI matrix (`cargo build --workspace` + the `--lib`/`--lib --bins` test matrix) is **green under `RUSTFLAGS=-D warnings --locked`**.

- **P3.2 (XS)** — removed the dead `loop` in `assets.rs::import_bytes_with_provenance` (now a straight `match`; `provenance` moved not cloned). Behavior-preserving (every arm already diverged); clears the deny-by-default `clippy::never_loop`. Reviewer: no defect.
- **P2.5 (XS)** — removed the soft→hard route-override copy in `generate_image`/`generate_audio` (mirrors the P0 `edit_image` fix). New e2e test `daemon_http_generate_image_falls_back_when_run_provider_lacks_image_backend` — **teeth-proven**: reintroducing the bug makes it fail with `no image-generation backend is configured for route scripted`; the fix makes it pass.
- **P2.3 (S)** — replaced the fixed-window limiter with a token bucket (`ObservationIngressRateLimitState::admit`, pure + injectable `now_ms`). 7 unit tests incl. an instantaneous-boundary-burst regression and a divide-by-zero/clock-skew guard. Existing rate-limit e2e test stays green (tokens aren't spent on pre-limiter rejections). Reviewer: math provably bounded (`retry_after ∈ [1, window_ms]`), no stale callers.
- **P1.5-S (S)** — `load_or_create_signer` now emits a loud startup warning when the audit signing key falls back to the co-located state-root path, plus an "Integrity caveat" in `docs/operators/security-and-auth.mdx`. Fires only on the co-located fallback (env-key path returns earlier).
- **P3.6 (S)** — `resolve_control_plane_auth_config` now warns when control-plane auth resolves to disabled (both loopback sites). Added the one missing security-contract test (`none` + loopback) — the rest of the matrix is already covered by `main.rs` tests.
- **P0.2 (S)** — `.github/workflows/ci.yml`: `fmt` + `build & test` (`-D warnings`, `--locked`, daemon runs `--lib --bins` so the CLI/serve tests are covered) + advisory `clippy`. Toolchain `dtolnay/rust-toolchain@stable` (edition 2024 needs ≥1.85). Validated by running the exact commands locally.

Not in this batch (S–M or larger, per the "S and XS only" scope): P1.1, P1.2, P1.3, P1.4, P2.1, P2.2, P2.4(full), P3.1, P3.3, P3.4, P3.5.

**Discovered during verification — flaky test (new follow-up, ~S):** `daemon_exposes_runtime_reconfiguration_and_global_sse` (`tests.rs:40830`) is timing-flaky under parallel-suite load: it passed 3/3 in isolation and in an earlier full matrix, but failed once in a loaded full `--lib --bins` run with a 5s SSE-chunk timeout. Root cause: the test subscribes to `/v1/events/stream` *live* and then POSTs the change, so an event broadcast before the subscription registers is missed. It is unrelated to this batch's changes (none touch SSE/runtime/events). Fix: use the existing cursor mechanism — capture the event id before the mutation and open the stream with `Last-Event-ID` (the handler already supports `subscribe_after(cursor)` at `handlers.rs:4848`), then read until the expected event. This matters for **P0.2 CI reliability**: until fixed, the CI `build & test` job can intermittently redden on this one test (re-run passes).

## Implementation log — 2026-07-03 (M batch)

- **P1.1** — `anthropic.rs` now builds requests from a model-capability table (`anthropic_model_uses_adaptive_thinking`, one line per family): Fable 5 / Mythos 5 / Opus 4.7 / 4.8 / Sonnet 5 get `thinking:{type:"adaptive"}` + `output_config.effort` and omit `temperature`; legacy `budget_tokens` configs fold onto the nearest effort tier; a requested summary maps to `display:"summarized"`; ≤4.6 keeps the old shape. 6 unit tests incl. Bedrock-prefixed ids and the sonnet-5 vs sonnet-4-5 non-match.
- **P1.3** — full workspace migration off poison-prone `std::sync::{Mutex, RwLock}` to `parking_lot` (~700 call sites across kheish-runtime/daemon/agent/auth/core/harness/mcp, zero occurrences left; `tokio::sync` untouched; `assets.rs` Condvar migrated). The trigger was live evidence: one stale test expectation poisoned the daemon suite's debug-env mutex and cascaded 21 collateral failures.
- **P2.1 Phase 0** — all three parts landed:
  - *Incremental journaling:* new `JournalSink` trait on the engine, flushed at three turn boundaries (input/turn-top, after assistant+ToolCallStarted before execution, after ToolCallFinished), one fsync per boundary via `FileSessionStore::append_records`; a sink failure fails the run cleanly. Runtime persist paths skip already-flushed entries via `journal_flushed_len`; `load_after` skips duplicate event offsets defensively.
  - *Torn-trailing-line tolerance:* readers skip a JSON-syntax failure only on the final non-empty line (warn); mid-file corruption still errors. The appender heals a torn tail before writing (truncate to last newline) so a new record never glues onto a partial line, and rolls back short writes.
  - *O(Δ) persist:* `append_batch_dedup` now dedups against a backwards tail read (`load_record_tail`) instead of re-parsing the whole transcript.
- **SSE flaky test** — fixed with the designed cursor approach: subscribe with `Last-Event-ID` (epoch 0 first, then the first event's id), read-until-match; race eliminated structurally.
- **Harness goldens** — the 2 red `kheish-harness` fixture tests were stale goldens: the working tree's engine changes (prompt-window generation on compaction) legitimately changed `canonical_state_digest`; regenerated goldens after verifying the diff was digest+prompt-window-only.
- **Startup worktree GC** — new `gc_orphaned_daemon_worktrees_on_boot`: reclaims daemon-owned worktrees whose agent record is settled/closed (crash between settle and cleanup), falls back to directory removal when the git registration is already gone, then `git worktree prune`. Live sidechains keep their worktrees. Covered by a restart e2e test.
- **kheish-auth redaction flake root-caused** (see below) — deliberate fix still pending.

**More load-dependent flakes observed 2026-07-03 (pre-existing, each passes in isolation, one red per loaded full-workspace run):**
- `kheish-auth redaction::tests::{auth_record_debug_redaction_tokens_include_only_secret_material, ephemeral_debug_redaction_tokens_are_bounded}` — both assert on the **process-global** redaction-token registry while serializing only against each other (`redaction_test_lock`, `redaction.rs:173`). Every other kheish-auth test that constructs an `AuthManager` mutates that same registry without the lock (`manager.rs:87` on load, `:623` on refresh; `broker.rs:489` registers ephemeral tokens), so a parallel test thread can replace the registry between the test's write and its read. Fix options (deliberate choice needed, ~S–M): share one test lock across every registry-touching test, or make the registry storage thread-local under `#[cfg(test)]` (careful: multi-thread tokio tests hop threads). Note: kheish-auth is **not** in the CI matrix today, so this cannot redden CI — but the matrix gap itself is worth closing.
- `kheish-daemon tests::daemon_stop_task_kills_orphaned_pipe_holder_and_is_session_scoped` — process-spawn/kill timing under load; passes in isolation. Not yet root-caused.
- `kheish-daemon tests::{daemon_http_runs_do_not_infer_over_explicit_invalid_edit_image_ids, daemon_http_runs_reject_non_image_edit_assets}` — failed together in 2 of 6 loaded full-suite runs on 2026-07-03, pass in isolation and in the other 4 full runs. Same edit_image family; suspected shared-fixture timing. Not yet root-caused.

---

## P0 — Baseline: green tree + CI (do first, together)

The 2 red tests existed **because there is no CI**. Fixing them without adding CI just resets a clock.

### P0.1 — Two failing tests `[x] applied`
Both failed on committed `main`; both now pass (full `kheish-daemon --lib` = 1232/1232 green).

- **`openapi_get_routes_are_classified_by_control_plane_auth`** — the 3 `/v1/observation-transcripts` GET routes were registered (`handlers.rs:574`) and OpenAPI-declared (`handlers.rs:2224`) but never classified in `auth.rs`. **Fix:** classify as **ReadOnly** (sibling consistency — the whole observation data plane is ReadOnly; raw audio bytes stay Admin-gated separately via `["","v1","assets",_,"raw",..]`). 3 additive match arms after `auth.rs:442`.
- **`daemon_http_runs_preserve_multi_image_edit_order`** — `edit_image` (`views.rs:675`) copied the run's **text** provider into `request.route.provider`, turning a soft preference into a **hard route override**; a multi-image edit on route `"scripted"` found no image backend, errored silently, recorded `[]`. **Fix:** drop the hard-force; `preferred_route_id` is already passed to `edit_with_context` as the soft preference (preferred→default→any fallback). An explicit provider in the model's tool call is still honored as a hard override.

Files: `crates/kheish-daemon/src/api/auth.rs`, `crates/kheish-daemon/src/state/views.rs`. (Also applied: 2 rustc `unused_variables` warning fixes — `Err(error)`→`Err(_)` in `connectors/ingress/telegram.rs:312` and `connectors/output/telegram.rs:488`.)

### P0.2 — CI `[ ]`
`.github/workflows/ci.yml` with pinned `dtolnay/rust-toolchain@1.96.0` (edition 2024), `Swatinem/rust-cache`:
- `fmt` — `cargo fmt --all --check` (clean today).
- `build-test` — `cargo build --workspace --locked`, then the AGENTS.md `--lib` matrix (`kheish-types`/`-core`/`-runtime`, `-coding-tools`, `-daemon`). `RUSTFLAGS=-D warnings` (tree is rustc-warning-clean after the telegram fixes). Green today.
- `clippy` — **advisory (`continue-on-error: true`)**. It cannot block yet: a deny-by-default `never_loop` at `assets.rs:396` makes even plain `cargo clippy` fail to compile `kheish-daemon`, plus ~hundreds of warn-level lints. See P3.5.

Live provider tests (`*_live.rs`) live in `tests/` (excluded by `--lib`) and self-skip without API keys, so CI needs no secrets.

---

## P1 — High-severity, user-reachable, bounded fixes (best ROI)

### P1.1 — Anthropic request-shape 400 on current models `[ ]`
`build_request_body` sends `thinking:{type:"enabled",budget_tokens:N}` (`anthropic.rs:217`) **and** unconditional `temperature` (`anthropic.rs:259`). Both are rejected with HTTP 400 on Opus 4.7/4.8, Sonnet 5, Fable 5 (confirmed against the authoritative API reference). Any reasoning request on a current Claude model fails.
**Fix (model-capability-driven):** mirror the existing `openai_model_supports_temperature` precedent (`openai.rs:1644`). For ≥4.7/Sonnet 5/Fable 5 → emit `thinking:{type:"adaptive"}` + `output_config.effort` and **omit** sampling params; for ≤4.6 keep `budget_tokens`/temperature. Route both through single chokepoint helpers so future model rules are a one-line table change. Effort S–M. Risk low (additive capability table; existing OpenAI test pattern to copy).

### P1.2 — OutputHost untargeted fan-out `[ ]`
`OutputHost::deliver()` (`output/lib.rs:88`) broadcasts an empty-target envelope to **every** registered plugin. Reachable **today** via `runtime.rs:2543-2569` for completed runs with no reply target → a finalized response leaks across Slack + Telegram + webhook. (`output_workflow.rs:197` already guards; `runtime.rs` does not.)
**Fix:** fail-closed — on empty targets, `warn!` + deliver to none (matching `output_workflow.rs:197`); add an explicit `deliver_broadcast(envelope)` for the rare intentional case; relabel the `runtime.rs` audit trace from `"broadcast:broadcast"` to `"skipped:no_targets"`. The output is already persisted locally, so no data loss. One existing test (`lib.rs:196`) encodes the footgun and must be flipped. Effort S–M. Risk medium (deliberate behavior change — evidence says only the runtime completed-run path is affected, which is safe to no-op).

### P1.3 — Lock-poison cascade `[ ]`
79 production `std::sync::Mutex/RwLock .lock().expect("poisoned")` sites (kheish-runtime 31, kheish-daemon 23, kheish-agent 23 all in `supervisor.rs`, kheish-mcp 1, kheish-auth 1). A panic while any guard is held poisons the lock; every later `.expect()` aborts.
**Fix:** swap those **sync** sites to `parking_lot::{Mutex, RwLock}` (non-poisoning, returns guard directly; compiler rejects a leftover `.expect()` on a non-`Result` guard). `parking_lot 0.12.5` is **already vendored** transitively (via `string_cache`), so promoting it to a direct dep adds **zero** new crates. Leave all 325 `tokio::sync::Mutex .await` sites alone. Start with `supervisor.rs` (23 sites) + runtime hot paths. Effort M. Risk low (mechanical, behavior-preserving, compiler-checked). Fallback with no new dep: a `lock_recover()` extension trait over `PoisonError::into_inner`.

### P1.4 — Auth-store at-rest weaknesses `[ ]`
`store.rs` uses AES-256-GCM-SIV with **no AAD** (encrypt 189, decrypt 224) and a `load()` that **silently accepts plaintext** credentials (`contains_key "ciphertext"` at 131).
**Fix:** envelope **v2** binding AAD over version+label (explicitly **not** path); warn + auto-migrate plaintext with opt-in `KHEISH_AUTH_STORE_REQUIRE_ENCRYPTED` to hard-fail. **Critical constraints:** the v1 decrypt branch stays byte-for-byte untouched, and `RawKey` stays raw-byte-identical → no decryption lockout for existing stores. Effort M. Risk medium-gated by the no-lockout constraints + the existing migration framework.

### P1.5 — Audit-signing-key honesty `[ ]`
The Ed25519 hash-chained external-action audit's signing key is co-located with the state it protects (`audit-signing.key` under `state_root`, `external_action.rs:624`). Co-location weakens tamper-evidence (an attacker with state-root write can forge + re-sign).
**Fix:** (S, now) loud startup warning + honest docs that integrity holds only if the key is moved off-box; (M, next) a `--audit-signing-key-file` flag (env override already exists at `external_action.rs:1052`) + a verify-before-delete `relocate` command that preserves `key_id` so the existing chain stays verifiable. Effort S then M. Risk low.

---

## P2 — Real gaps, larger or lower-reachability

### P2.1 — Mid-run durability, Phase 0 (the headline gap, honest-minimal) `[ ]`
Today: journaling is batched at end-of-run (`runtime.rs:2444` appends `journal[previous..]`), the error path persists nothing (`runtime.rs:1377`), a `Running` `Input` run is marked **`Interrupted`** on restart (`services/run.rs:2767`), a torn trailing JSONL line **bricks the whole load** (`store.rs:391` `?`), and every persist re-reads the whole file → O(n²)/session (`store.rs:282`).
**Phase 0 (foundation, no auto-resume):**
- **Incremental journaling** at 3 turn boundaries — after `accept_input` (persist `InputReceived` + durable run-meta), after assistant msg + all `ToolCallStarted` and before tool execution, after `ToolCallFinished` — one fsync each. Via an `Arc<dyn JournalSink>` on the engine.
- **Torn-trailing-line tolerance**: parse loops skip a parse failure **only** on the final non-empty line (warn + truncate to last good line); a middle-line failure still errors. Append side captures file length and `ftruncate`s back on short-write so "torn line ⇒ trailing" becomes a true invariant.
- **O(Δ) persist**: store-owned persisted cursor (`SessionRestoreCursor` already exists) → `append_after_cursor` appends only new records, no whole-file reparse; keep `max_suffix_prefix_overlap` only as one-time restore reconciliation.

**No new on-disk types → envelope stays v2 → zero migration.** Phase 0 alone stops silent loss of in-flight work, fixes the O(n²) persist, and de-bricks torn files — making the README durability claim honest even before auto-resume. Effort S–M (cross-crate; moves I/O onto the hot path — must fail the run cleanly, never panic).

### P2.2 — Google provider non-streaming `[ ]`
`google.rs stream()` POSTs `:generateContent` and buffers the full body via `response.json()` (`google.rs:658`); nothing calls `mark_activity()`, so any generation >90s trips the inactivity timeout (`model.rs:335` vs `builders.rs:60`), retries, repeats, hard-fails — a **correctness** bug, not just UX.
**Fix:** convert to `streamGenerateContent?alt=sse` reusing `providers/sse.rs`, emitting `mark_activity` per chunk. Effort M.

### P2.3 — Rate limiter ~2× burst `[ ]`
`reserve_ingest_slot` (`observation.rs:996`) is a fixed-window counter — `burst` at end of window N + `burst` at start of N+1 pass within < `window_ms`. It is the **only** fixed-window limiter in the repo.
**Fix:** token bucket mirroring the two existing correct limiters (`connectors/runtime.rs:34`, `connectors/routes/mod.rs:38`); `now_ms` already injected → directly unit-testable. Effort S.

### P2.4 — `observation.rs` zero unit tests `[ ]`
1896 lines of security-critical auth/retention/idempotency logic, **0 `#[test]`**. Add ~15 in-file cases over tempdir stores: `authorize_upload_token` (grace/revoke/expired matrix, `:935`), `reserve_ingest_slot` boundary regression (locks in P2.3, `:996`), `find_by_ingest_key` reuse-different-fingerprint (`:1153`), `enforce_source_retention` eviction order — byte/count/TTL + protected-asset (`:1412`). Effort M. Risk none (additive).

### P2.5 — Companion image-route bug `[ ]`
The exact `edit_image` hard-force pattern fixed in P0.1 also lives in `generate_image` (`views.rs:595`) and `generate_audio` (`views.rs:634`). Same one-block removal; no test covers them yet (add coverage alongside). Effort XS.

---

## P3 — Strategic, larger, or low-severity

### P3.1 — Mid-run durability, Phase 1 (auto-resume) `[ ]`
Fully delivers the "resumed by the daemon" headline. `resume_in_flight_run` re-enters `drive_loop`; daemon `recover_running_record` gains a resume branch + `RunEvent::Resumed` (new `restart_resume_allowed`, kept distinct from `restart_requeue_allowed`); orchestrator gets `SessionCommand::ResumeInFlight`.
**Safe-resume crux:** the existing `ToolCallStarted`/`ToolCallFinished` split gives a 3-state classification on restart — *finished durably* (safe), *never started* (safe to re-issue), *started-not-finished* = **ambiguous**. Resolve ambiguous by **synthesizing an `interrupted` `ToolCallFinished`** (mirroring `denied_tool_result`) and letting the model decide — **never blindly re-run** a non-idempotent `bash`/`apply_patch`. Idempotent final-output dispatch via the existing `payload_digest` Output-record dedupe.
Effort L, cross-crate, **changes observable semantics** (runs that ended `Interrupted` now continue to `Completed`) → gate behind a config flag. **Do only after Phase 0 proves out.** Rejected alternative: adding `Input` to `restart_requeue_allowed` re-runs every side effect from scratch — unsafe, which is why `Input` is excluded today.

### P3.2 — `never_loop` at `assets.rs:396` `[ ]`
A `loop` whose every arm returns/bails — a genuine latent dead-loop bug (and the deny-by-default clippy error blocking the clippy gate). Rewrite as a plain block/`match`. Effort XS.

### P3.3 — Auth-store KDF `[ ]`
argon2id for **passphrase-derived** keys only (raw keys unchanged → no lockout). New dep `argon2`. Layers onto P1.4's v2 envelope. Effort M.

### P3.4 — Provider dedup + Retry-After bonus bug `[ ]`
Extract shared provider free functions into `providers/common.rs`. **Do the retry-after unification first** — OpenAI & OpenRouter parse only integer-seconds `Retry-After` and **silently ignore the HTTP-date form** (Anthropic & Google handle both). The dedup is polish; the retry-after fix has independent value. Effort M.

### P3.5 — Clippy debt → blocking gate `[ ]`
Flip `never_loop` (P3.2), then clear ~hundreds of warn-level lints (or curate a `[workspace.lints]` allow-list) — notably **31 `MutexGuard`-held-across-await** (latent deadlock/hang risk; see theme 1). Only then make the P0.2 clippy job blocking. Effort L.

### P3.6 — Auth-off loopback warning `[ ]`
Gap 3 was mostly **refuted** — non-loopback bind without an admin token is already refused (`serve/support.rs:255`, `builders.rs:540`→`config.rs:679`). Residual: a silent loopback case → warning + belt-and-suspenders guard. Effort S.

---

## Cross-cutting themes

1. **Lock discipline appears twice.** P1.3 (79 poison-prone `std::sync` sites) and P3.5 (31 clippy `MutexGuard`-held-across-await) are different primitives, same class of concern. Treat as one workstream: P1.3 removes the poison-abort failure mode; the await-holding audit removes a latent deadlock/hang risk.
2. **Honesty vs. claims.** Durability (P2.1/P3.1), audit-key separation (P1.5), and the plaintext-tolerant auth store (P1.4) are all places where the *claim* outruns the *guarantee*. Phase-0-only + honest docs closes the gap cheaply before full auto-resume lands.
3. **Silent-failure pattern.** Both red tests were swallowed-error → empty-result → still "completes" bugs, and the `edit_image` override recurs (P2.5). A sweep for other "swallow error, return empty, report success" sites is worthwhile.

## Recommended sequencing

1. **Land P0** (green tree + CI) — measure everything else against a green baseline.
2. **P1 batch** — independent, parallelizable, all S–M, highest ROI. Within: P1.1 (broken core path) → P1.2 (leak) → P1.3 (availability) → P1.4/P1.5 (security).
3. **P2.1 Phase 0 durability** — the honest-minimal thesis fix; high value, contained.
4. **Remaining P2** (Google streaming, rate limiter + tests, companion bug).
5. **P3** as capacity allows; **P3.1 only after Phase 0 is proven**, behind a flag.

---

## Appendix — key files

- Tests/CI: `crates/kheish-daemon/src/api/auth.rs`, `crates/kheish-daemon/src/state/views.rs`, `.github/workflows/ci.yml` (new), `crates/kheish-daemon/src/assets.rs:396`.
- Providers: `crates/kheish-runtime/src/providers/{anthropic.rs:171,217,259, openai.rs:1644, google.rs:601,658, sse.rs, common.rs(new)}`, `crates/kheish-runtime/src/model.rs:326`.
- Output: `crates/kheish-output/src/lib.rs:82,196`, `crates/kheish-runtime/src/runtime.rs:2466,2543`, `crates/kheish-daemon/src/state/output_workflow.rs:197`.
- Locks: `crates/kheish-agent/src/supervisor.rs:2,66`, runtime/daemon sync sections.
- Auth/audit: `crates/kheish-auth/src/store.rs:120,184,224`, `crates/kheish-daemon/src/services/external_action.rs:624,1052`.
- Durability: `crates/kheish-core/src/engine.rs` (`drive_loop`, `execute_finalized_batch:1077`, `resume_with_pending_batch_and_restoration:462`), `crates/kheish-runtime/src/runtime.rs:1334,2444`, `crates/kheish-session/src/{store.rs:273,359,391, fs.rs:115}`, `crates/kheish-daemon/src/services/run.rs:2715,2897`.
- Observation: `crates/kheish-daemon/src/services/observation.rs:935,996,1153,1412`.
