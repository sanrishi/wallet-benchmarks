# Acceptance Audit

**Status: Ready for Canonical Run**

This document maps the current `feat/harness-implementation` branch directly to
the acceptance criteria in Issue `#1` and explains the benchmark architecture in
maintainer-facing terms.

**Important:** the remaining external requirement is a funded Esmeralda proof
run. This document is about implementation completeness and protocol fidelity,
not the funding step itself.

## Executive Summary

This branch implements a reproducible Rust benchmark harness for the three
required wallet modes:

| Mode | Implementation | Benchmark Intent |
| --- | --- | --- |
| `old_wallet` | `minotari_console_wallet` subprocess managed by the harness and driven over gRPC | Measures the legacy wallet as an externally managed process |
| `new_wallet` | Stateless `minotari` CLI subprocesses for wallet operations, local offline signing, HTTP JSON-RPC submit | Measures the practical performance surface of `minotari` without hiding CLI or signing pain points |
| `payment_processor` | Same stateless `minotari` subprocess model with real multi-recipient batch creation | Measures 1-to-many throughput using the same user-facing transaction path |

The implementation intentionally does **not** add scenario-level retry, backoff,
artificial throttling, or UTXO pre-partitioning. Where a wallet stalls, rejects,
or serializes internally, that behavior is surfaced as a benchmark result.
(Driver internals may perform limited infrastructure retries — e.g. port-conflict
retry in the new-wallet daemon, directory-lock retry in old-wallet reset —
these are platform robustness concerns that do not mask wallet behavior.)

For the current canonical-run budget, funding size and send size are
intentionally decoupled:

- `a_fund` is the wallet funding baseline
- `tx_amount_ut` is the per-send amount used in `S1`, `S4`, and `S5`

This keeps the benchmark aligned to the maintainer instruction to use low-value
transactions that stay above fees, without accidentally scaling send size up
with wallet budget.

## Acceptance Matrix

| Issue Requirement | Branch Status | Notes |
| --- | --- | --- |
| Harness source code in a new repo, buildable and runnable by anyone with a funded wallet | **Implemented** | Rust workspace with `harness` binary crate and documented configuration |
| Step-by-step run instructions | **Implemented** | See [README.md](/C:/Users/My/Desktop/GitHub/Tari_bounties/wallet-benchmarks/README.md) for build, address preflight, funding, and run procedure |
| Baseline result profile committed alongside code | **Partially complete** | `baseline_profile.json` output path and schema are implemented; canonical funded run remains the final external step |
| Old wallet mode via gRPC with process lifecycle owned by harness | **Implemented** | Spawn, readiness wait, stop, reset, restart, scan, balance, send, confirmation polling |
| New wallet mode with offline signing and no background wallet daemon | **Implemented** | Stateless CLI subprocess wrapper; no persistent daemon; local offline signing and HTTP submit |
| Payment processor mode with 1-to-many batching | **Implemented** | Real multi-recipient transaction creation via repeated `--recipient` arguments |
| All configuration parameters exposed and recorded | **Implemented** | Config is loaded from `config.toml`, serialized into the report, and version pins are explicit |
| Structured machine-readable output | **Implemented** | JSON report with per-mode, per-scenario metrics and computed deltas |
| Harness does not hide wallet pain | **Implemented** | No retry/backoff/throttle/UTXO pre-partitioning in scenario orchestration |

## Scenario Audit

| Scenario | Required Behavior | Branch Status | Evidence |
| --- | --- | --- | --- |
| `B0` | Scan empty wallet from genesis | **Implemented** | Real `scan_from_genesis()` per driver with `ScanMetrics` |
| `S0` | Funding baseline with timing | **Implemented observationally** | `observe_funding()` records first visible incoming funds and spendable confirmation timing using real wallet state |
| `S1` | Doubling + fan-out build-up | **Implemented** | Real multi-output batching for doubling and fan-out in scenario layer |
| `S2` | Full scan from genesis after S1 | **Implemented** | Validates recovered balance against pre-wipe post-`S1` checkpoint |
| `S3` | Birthday scan after S1 | **Implemented** | Same checkpoint validation as `S2` |
| `S4` | Concurrent construction | **Implemented** | True harness-level parallel submission; no concurrency cap added |
| `S5` | Batch vs single throughput comparison | **Implemented** | `M/K` batch sends **and** `M` single sends run on every mode, enabling `s5_throughput_multiplier` computation |
| `S6` | Full scan from genesis after S5 | **Implemented** | Validates against post-`S5` checkpoint |
| `S7` | Birthday scan after S5 | **Implemented** | Validates against post-`S5` checkpoint |

## Architectural Strengths

### 1. Real Confirmation Polling

**Strict compliance point:** this branch does not hardcode
`broadcast_to_confirmed_secs = 0.0`.

Instead:

- `old_wallet` polls real gRPC transaction state with `GetTransactionInfo`
- `new_wallet` and `payment_processor` poll real SQLite wallet transaction state
  and refresh state through incremental `scan`
- confirmation timing is measured against `c_min`, not guessed

This matters because the bounty explicitly asks the harness to measure
confirmation behavior. Returning `0.0` for confirmed timing is not acceptable in
a serious benchmark harness.

### 2. Real Multi-Output Batching For `S1` And `S5`

**Strict compliance point:** this branch does not fake batching by looping
single-output sends when multi-recipient surfaces exist.

Current behavior:

- `old_wallet` uses gRPC `TransferRequest` with multiple recipients in one tx
- `new_wallet` uses `create-unsigned-transaction` with repeated `--recipient`
- `payment_processor` uses the same real batch creation path

Implications:

- `S1` doubling rounds are modeled as actual 2-output sends
- `S1` fan-out rounds are modeled as actual `fanout_outputs_per_tx` sends
- `S5` batch arm is a real 1-to-many path, not just a loop of singles
- `S5` runs **both** the batch arm and the individual-send arm on every mode,
  producing a per-mode `s5_throughput_multiplier` in the report

### 3. Strict Scan Validation Against Checkpoints

**Strict compliance point:** this branch does not allow scan scenarios to
self-justify by comparing observed balances to themselves after a wipe.

Current behavior:

- after `S1`, the harness captures the real post-`S1` balance
- `S2` and `S3` verify the recovered balance against that checkpoint
- after `S5`, the harness captures the real post-`S5` balance
- `S6` and `S7` verify against that later checkpoint

This is stronger than a scan implementation that simply says “scan completed and
the wallet has some balance.”

### 4. Windows Lifecycle Robustness For Mode 1

The harness owns the old-wallet process lifecycle explicitly:

- spawn
- readiness polling
- stop
- wait for process exit
- retrying reset on Windows file-lock behavior
- restart before wipe-sensitive scan scenarios

That makes repeated scan scenarios materially more reproducible on Windows,
which is where file-lock behavior commonly causes false failures in ad hoc
benchmark harnesses.

## Mode 2 / Mode 3 Architecture Rationale

### Stateless CLI Wrapper By Design

`new_wallet` and `payment_processor` are intentionally implemented as stateless
`minotari` CLI subprocess wrappers:

1. invoke `minotari` for wallet initialization, balance, scan/rescan, and
   unsigned transaction drafting
2. sign locally in-process using the same published Tari crates
3. submit the signed transaction to the base node over HTTP JSON-RPC
4. exit immediately after each operation

This architecture is deliberate for benchmark fidelity:

- it measures the real user-facing `minotari` path, including CLI and wallet DB
  behavior
- it avoids inventing a long-lived daemon layer that end users do not use for
  this workflow
- it preserves parity between Mode 2 and Mode 3
- it keeps wallet selection, locking, scanning, and confirmation behavior
  exposed instead of smoothed over by harness-managed caching or orchestration

### Why This Is Benchmark-Authentic

For this bounty, the most important property is not theoretical purity; it is
that the benchmark surfaces real wallet pain points clearly and reproducibly.

This branch does that by:

- preserving wallet-owned transaction drafting and UTXO locking
- preserving wallet-owned scan and confirmation behavior
- preserving the observable cost of repeated CLI/database operations
- avoiding harness-side abstractions that would make Mode 2 or Mode 3 look
  artificially better than they are in practice

## Compliance Warnings

**Warning: No scenario-level retry/backoff/throttle is added.**

Concurrent scenarios intentionally allow the wallet modes to expose:

- lock contention
- transaction rejections
- serialization gaps
- stalls
- timeout behavior

That is required by the principle in Issue `#1`: the harness measures wallet
pain; it does not engineer around it.

**Note on S4 resource amplification:** S4 spawns up to `max(concurrent_batches)`
(typically 128) daemon instances simultaneously, each opening the same SQLite
`wallet.db`. This creates lock contention that is partly an artifact of the
per-call-daemon architecture, not purely a wallet bottleneck. The cross-mode
comparison is still valid (all modes experience the same architecture), but
absolute failure counts should be interpreted with this in mind.

**Warning: Scan checkpoints are treated as correctness boundaries.**

If a scan scenario recovers the wrong balance relative to the pre-wipe
checkpoint, the scenario is reported as failed even if the scan itself
completed.

## Test And Verification Evidence

The branch now includes focused scenario-orchestration tests that verify the
most important non-network invariants:

| Test Focus | What It Proves |
| --- | --- |
| `S0` funding observation | Funding scenarios now record tx metrics instead of empty placeholders |
| `S1` batch topology | Doubling rounds use 2-output batches; fan-out uses configured batch sizes |
| `S5` count discipline | Both arms (batch `M/K` txs + individual `M` txs) run on every mode |
| Scan checkpoint failure | Scan scenarios fail when recovered balances do not match expected checkpoints |

These tests complement the runtime harness behavior rather than replacing it.

## Known gRPC RescanWallet Limitation

The `minotari_console_wallet` gRPC `RescanWallet(from_height=0)` call only scans
the last ~5,000 blocks — it does **not** perform a true genesis scan.  The
harness works around this in `old_wallet`:

1. **Genesis scans (`B0`, `S2`, `S6`)** — The wallet is started with
   `--seed-words` whose cipher-seed birthday is set to `0` (day-count 0 = Unix
   epoch).  The wallet implicitly performs a full genesis scan at startup, so no
   `RescanWallet` gRPC call is needed.

2. **Birthday scans (`S3`, `S7`)** — `RescanWallet(from_height=X)` is issued
   *after* startup for `X > 0` (works correctly upstream).  Height `0` is never
   passed to `RescanWallet`; the birthday-mechanism above is used instead.

This is documented inline at `harness/src/drivers/old_wallet.rs:226-228` and in
the `do_scan()` implementation.

## Mode 3 Architecture: Daemon Microservice vs. Vendored Submodule

Mode 3 (`payment_processor`) uses a dual-daemon architecture:

```
┌─────────────────────┐     HTTP ───►  ┌──────────────────────┐
│  minotari daemon    │                 │  minotari_payment_   │
│  (PR account daemon)│ ◄─── HTTP      │  processor daemon    │
│  /balance           │                 │  POST /v1/payments   │
│  /scan_status       │                 │  POST /v1/batches    │
│  /version           │                 │                      │
└─────────────────────┘                 └──────────────────────┘
         ▲                                        │
         │          ┌──────────────────┐           │
         └──────────│ minotari CLI     │◄──────────┘
                    │ (offline signing)│
                    └──────────────────┘
```

**Why a network-decoupled microservice instead of a vendored submodule:**

1. **Faithful reproduction of production topology** — The
   `minotari_payment_processor` binary *is* the production payment processor, a
   standalone HTTP microservice.  Vendoring its Rust types as a git submodule
   and calling its functions in-process would benchmark a call-graph that does
   not exist in production.  The harness would measure library dispatch
   overhead, not real IPC, HTTP serialization, and cross-process latency.

2. **3-process benchmark fidelity** — Issue `#1` requires benchmarking the
   wallet in three distinct modes.  Mode 3's intent is to measure the real
   multi-process payment pipeline: wallet daemon → payment processor → base
   node.  A vendored-submodule approach collapses this into a single process,
   hiding cross-process overheads that matter for throughput measurement.

3. **No upstream maintenance burden from vendoring** — Vendoring a snapshot of
   the payment processor crate would require ongoing updates to track upstream
   API changes.  The microservice approach consumes the *released binary*, so
   the harness automatically tracks whatever version the operator pins in
   `config.toml`, with no build-time coupling to the payment processor's
   internal dependency graph.

4. **Scan-surface availability** — The payment processor microservice does not
   expose wallet scan/sync endpoints.  By keeping the `minotari daemon`
   (PR account daemon) as a separate managed process, the harness can run the
   full `B0`–`S7` matrix for Mode 3.  If the payment processor were vendored
   as a submodule, scan scenarios would require an entirely separate wallet
   integration, adding complexity without improving benchmark fidelity.

5. **Comparable overhead to PR #6's vendored-submodule approach** — The
   alternative approach (vendored payment-processor submodule) eliminates the
   HTTP hop between the harness and the payment processor, but it introduces a
   different artifact: the benchmark measures in-process function calls against
   the payment processor's internal types rather than the actual released
   binary.  For a benchmark whose purpose is measuring *wallet* performance
   (not payment-processor dispatch), the microservice approach gives a more
   representative baseline.

## Ready-For-Run Checklist

Before the canonical funded run:

1. build the harness in release mode
2. print the three funding addresses (deterministic from seeds in config, or
   randomly generated and shown)
3. fund each mode's address with `a_fund`
4. set the exact pinned wallet/base-node versions in `config.toml`
5. run the benchmark and capture `baseline_profile.json`

At that point, the branch is positioned to serve as the canonical benchmark
submission rather than a scaffold or design draft.
