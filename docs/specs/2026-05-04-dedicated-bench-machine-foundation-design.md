# Dedicated benchmark machine — foundation (design)

Status: design approved 2026-05-04. Implementation pending.

## Goal

Stand up a single always-on dedicated Linux VM that becomes the canonical home for Chisel benchmark runs. Replaces the GitHub-shared runner as the primary surface for per-PR regression detection, hosts release-time canonical-numbers runs, and (once Spec 4 ships) hosts long-running soak/stability runs.

The dedicated machine solves three problems the GitHub-shared runner cannot:

1. **Trustworthy absolute numbers** for the README and release notes — shared-runner numbers vary ±15% between runs, which makes them useless as published artifacts.
2. **Actionable per-PR regression signal** — at the bench-diff binary's existing thresholds (5% throughput / 10% p99), shared-runner noise floor matches or exceeds the threshold, so the diff comment frequently flags drift that is purely runner variance. Dedicated hardware drops the noise floor below the threshold and makes the signal real.
3. **A place where multi-hour soak workloads can run at all** — GitHub-hosted runners have a 6-hour job timeout (configurable upward only on self-hosted), and the shared environment is unsuitable for the kind of stress workloads soak tests are supposed to exercise.

## In scope for this spec

The machine itself and the foundation that all three workload modes share:

- VM provisioning and OS setup
- Self-hosted GitHub Actions runner installation, registration, and lifecycle
- Setup-time noise-validation gate (go/no-go on the qualified instance)
- Workflow scaffolding pattern (one `bench-*.yml` per mode, all targeting the runner)
- Concurrency model
- Auth model
- Result-transport mechanisms (Releases, main-branch commit, Actions artifacts)
- Fork-PR security gate
- Operational basics: disk management, log retention, OS and runner updates, observability, fallback to the shared runner

## Out of scope (each gets its own spec/plan pair later)

- **Spec 2 — Canonical numbers workflow**: release-tag triggering, canonical artifact format, README/release-notes integration
- **Spec 3 — Per-PR low-noise regression detection**: the `bench.yml` migration, fallback-comment header rendering, runner-discovery details
- **Spec 4 — Soak / stability workloads**: new workload code in `bench/` (multi-hour fill cycles, crash-recovery loops, memory-pressure tests), schedule, failure semantics, artifact format
- **macOS coverage**: future second machine; this spec preserves the ability to add it without re-design

## Explicit non-goals for v1

- **No bare-metal hardware.** Provisioned-IOPS cloud VM is the chosen hardware tier; if the noise gate fails on the chosen instance, we try a different instance type or provider, not a different hardware class.
- **No precision-instrument noise floor.** Threshold is "below existing diff thresholds" (≤2% throughput / ≤5% p99 latency coefficient of variation), not microbenchmark precision. Microoptimization detection is not a v1 goal.
- **No GUI, dashboard, or static site.** Results are consumed via GitHub-native surfaces (Releases page, PR comments, files committed to `main`).
- **No multi-machine fleet.** Single machine. The future macOS extension is a second machine of similar shape, not a fleet manager.
- **No backup dedicated machine.** Per-PR fallback is to the GitHub-shared runner via runner-discovery, not to a backup dedicated machine.
- **No bench-blocked releases.** Per-PR diff comments and canonical numbers are signal, never gate. A failed bench never blocks a merge or a release.

## Success criteria — v1 ships when

1. The machine is provisioned, the Actions runner is registered, and `gh workflow run bench-dedicated.yml` against a known commit produces a `cross-engine.md` artifact within 30 minutes.
2. The setup-time noise gate passes on the chosen instance (≤2% throughput / ≤5% p99 across 5 back-to-back scenario runs).
3. The shared-runner `bench.yml` is migrated to use the fallback runner-label set; an artificially-induced offline state shows the fallback rendering in the PR comment.
4. **At least one of Specs 2/3/4 is implemented end to end on top of the foundation before declaring foundation v1 done.** This is a guard against shipping infrastructure that has no real consumer.

---

## Hardware

Single Linux cloud VM, provisioned-IOPS storage, dedicated tenancy where the provider offers it. Specific instance selection is **deferred to deployment time** — the noise gate is the actual qualification.

### Sizing requirements

| Resource | Minimum | Rationale |
|---|---|---|
| vCPUs | 4 | Bench is single-process / single-writer, but the runner agent + git/cargo + system load want headroom. 2 vCPUs is too tight; 4 leaves comfortable margin. |
| RAM | 8 GiB | Chisel's default cache is 8 MiB, but Criterion + scenario tier + page cache + system want room. Spec 4 soak workloads may want more — design preserves the ability to upsize without re-deploy. |
| Disk | 100 GiB SSD with provisioned IOPS ≥ 10K | Largest single scenario hit ~3 GiB on the shared runner; multiplied by 3 engines × multiple modes × held-for-comparison generations × cargo build artifacts, 100 GiB is comfortable. IOPS floor matters because noise comes mostly from storage, not CPU. |
| Network | 1 Gbps inbound, 100 Mbps outbound | Cargo download bursts, GitHub API calls, artifact uploads. Modest. |
| OS | Ubuntu 24.04 LTS | Matches `ubuntu-latest` closely enough that workflow YAML mostly works on both. Long-term security updates. |

### Provider candidates worth measuring

Non-exhaustive; picked at deployment time based on cost plus noise gate result:

- **Hetzner CCX-line** (dedicated vCPU, NVMe local storage) — cheap (~$30–80/mo for the qualifying tier); generally low noise on local storage. Best cost candidate.
- **AWS `m7i` or `r7i` with `gp3`/`io2`** — provisioned IOPS available, dedicated tenancy possible; more expensive (~$80–200/mo) but operationally familiar.
- **GCP `n2d` with balanced-PD** — comparable cost to AWS; slightly different noise profile.
- **Linode/DigitalOcean dedicated CPU** — lower cost than AWS/GCP; generally acceptable for non-precision work.

A v1.1 of the spec might pin a specific provider after deployment experience. v1 records the qualified configuration as a forensic artifact (see noise gate below).

## Noise-validation gate (setup-time)

A single shell script runs as the last step of provisioning, before declaring the machine ready:

1. Build the bench subcrate in release mode.
2. Run `cargo bench --bench scenarios` **5 times back-to-back** with no other load on the machine.
3. Parse `bench/results/scenarios_metrics.jsonl` from each run.
4. For each `(scenario, engine, mode)` cell, compute coefficient of variation (stddev / mean) for throughput and p99 latency across the 5 runs.
5. Compare against thresholds:
   - **Throughput COV ≤ 2%** (any cell exceeding fails)
   - **p99 latency COV ≤ 5%** (any cell exceeding fails)
6. Emit `noise-gate-report.md` showing per-cell COV.
7. Exit non-zero if any cell exceeds threshold; the deployment script aborts and the operator picks a different instance type or provider.

The gate is hard go/no-go. We refuse to put a noisy machine into production. The operator can re-run the gate after any environmental change (instance swap, provider swap, OS reinstall).

**5 runs, not 3 or 10.** 5 gives statistically meaningful COV at the bench's natural per-cell sample sizes (~50–500 ops per cell on the scenario tier) without making the gate prohibitively long (~50 minutes total at the macOS-equivalent runtime; faster on Linux). 3 is too few; 10 starts costing too much in operator time.

The successful `noise-gate-report.md` is committed to the repo at `bench-results/noise-gate/<UTC-ISO8601>-<provider>-<instance-type>.md` as a forensic record of the qualified configuration.

### Periodic re-validation (deferred)

A weekly cron job re-running the gate would catch cloud-provider noise drift, but: we don't yet know how often this matters; it adds a workflow + auth surface for issue-creation; it can be added later without changing anything else in the foundation. **Not in v1.** The design preserves the ability to add `bench-noise-monitor.yml` as a second cron-style workflow that piggybacks on the same runner.

---

## Trigger model and workflow architecture

### Workflow inventory

Four workflow files share the dedicated runner. Each has a single trigger source and a single purpose:

| Workflow file | Trigger | Mode it serves | Spec | Routes to |
|---|---|---|---|---|
| `bench.yml` (existing, modified) | `pull_request: branches: [main]` | (b) per-PR regression | 3 | `[self-hosted, linux, dedicated-bench, bench-v1]` with explicit fallback to `[ubuntu-latest]` |
| `bench-canonical.yml` (new) | `release: types: [published]` + `workflow_dispatch:` | (a) canonical numbers | 2 | `[self-hosted, linux, dedicated-bench, bench-v1]` (no fallback) |
| `bench-soak.yml` (new) | `schedule: cron: '0 2 * * 0'` (Sunday 02:00 UTC) + `workflow_dispatch:` | (d) soak / stability | 4 | `[self-hosted, linux, dedicated-bench, bench-v1]` (no fallback) |
| `bench-noise-monitor.yml` (deferred) | `schedule: cron: '0 3 * * 1'` (Monday 03:00 UTC) | Drift detection | future | `[self-hosted, linux, dedicated-bench, bench-v1]` |

Plus two operations workflows that don't run benches but live on the same runner:

| Workflow file | Trigger | Purpose |
|---|---|---|
| `bench-disk-cleanup.yml` (new) | `schedule: cron: '0 4 * * *'` (nightly 04:00 UTC) + `workflow_dispatch:` | Prune old `bench/results/<UTC>/` dirs; conditionally run `cargo clean` if free disk < 30 GiB |
| `bench-os-update.yml` (new) | `schedule: cron: '0 5 1 * *'` (monthly, 1st at 05:00 UTC) + `workflow_dispatch:` | Apply security updates; reboot if queue empty and a kernel patch is pending |

### Soak-schedule rationale

Weekly cadence (not nightly) keeps the soak workload from monopolizing the runner for big stretches and means a soak failure doesn't block the entire next week of per-PR feedback. Sunday 02:00 UTC is a low-contention slot — overnight in most timezones, with Monday morning available for triage. `workflow_dispatch:` allows manual runs on demand.

### Concurrency model

Every workflow that targets `dedicated-bench` declares the same workflow-level concurrency group:

```yaml
concurrency:
  group: dedicated-machine
  cancel-in-progress: false
```

This serializes all workflows on the runner: GitHub Actions queues newly-triggered jobs and runs them in strict arrival order. `cancel-in-progress: false` is deliberate — no preemption ever; an in-flight soak run finishes even if a per-PR job arrives.

Per-PR latency in a clash window is the remaining soak runtime plus the per-PR job's own runtime. We accept this rather than design a priority/preemption scheme. Revisit if it becomes a real pain point.

**v1 does not implement per-PR cancel-on-newer-push.** GitHub Actions only allows one workflow-level concurrency block; layering "cancel older runs of the same PR" on top of "serialize on dedicated-machine" requires a custom queueing layer. The runner is fast enough that the queue clears quickly; revisit if PR-thrash becomes a real problem.

### Runner labels and the fallback pattern

The dedicated runner registers with four labels: `self-hosted`, `linux`, `dedicated-bench`, `bench-v1`. The `bench-v1` versioned label allows rolling a parallel "next-gen" runner alongside the v1 one in the future without changing every workflow that targets it.

**GitHub Actions has no native "try this label set, fall back to that one" feature.** The fallback pattern is implemented explicitly via a sentinel detect-runner job:

```yaml
jobs:
  detect-runner:
    runs-on: ubuntu-latest
    outputs:
      use-dedicated: ${{ steps.check.outputs.use-dedicated }}
    steps:
      - id: check
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # Match the same label set the workflow's runs-on: uses below, so a
          # future macos-runner with dedicated-bench + bench-macos-v1 (but not
          # `linux` and not `bench-v1`) doesn't falsely satisfy this check.
          ONLINE=$(gh api repos/${{ github.repository }}/actions/runners \
            --jq '[.runners[]
                   | select(.status == "online")
                   | select([.labels[].name] | contains(["linux", "dedicated-bench", "bench-v1"]))
                  ] | length')
          if [ "$ONLINE" -gt 0 ]; then
            echo "use-dedicated=true" >> $GITHUB_OUTPUT
          else
            echo "use-dedicated=false" >> $GITHUB_OUTPUT
          fi

  bench-dedicated:
    needs: detect-runner
    if: needs.detect-runner.outputs.use-dedicated == 'true' && github.event.pull_request.head.repo.full_name == github.repository
    runs-on: [self-hosted, linux, dedicated-bench, bench-v1]
    steps:
      # ... full bench protocol ...

  bench-fallback:
    needs: detect-runner
    if: needs.detect-runner.outputs.use-dedicated == 'false' || github.event.pull_request.head.repo.full_name != github.repository
    runs-on: ubuntu-latest
    steps:
      # ... same bench protocol; comment-post step prepends a noise-warning header ...
```

The detect-runner step costs ~30 seconds on every PR. Acceptable for the resilience gain.

This pattern applies only to `bench.yml` (per-PR mode). `bench-canonical.yml` and `bench-soak.yml` have **no fallback** — they fail fast if the dedicated machine is offline:

- **Canonical:** for releases; the operator can re-trigger after the machine is back online (releases are infrequent enough that "wait for the machine" is acceptable). Canonical numbers from the noisy shared runner would defeat the purpose.
- **Soak:** failing scheduled runs surface via Actions failure notifications; operator triages.

### Fork-PR security gate

Every workflow targeting the dedicated runner adds an explicit `if:` check:

```yaml
if: github.event.pull_request.head.repo.full_name == github.repository
```

This skips the dedicated-runner job for PRs from forks. The fallback job (running on shared `ubuntu-latest`) doesn't need the gate — the existing `bench.yml` pattern already documents that fork-PR runs on the shared runner have read-only `GITHUB_TOKEN` and degrade gracefully.

The choice is `if:` rather than `pull_request_target` with required reviewers because: simplicity. `pull_request_target` solves a different problem (running with elevated privileges on PR-content commits), and required-reviewer environments add operator overhead for every fork PR. For a single-author repo where fork PRs are theoretical, the cheap defensive default is "skip dedicated-runner work for fork PRs entirely; the shared runner gives them the same diff comment they'd get today."

Fork PRs still get a (noisy) bench comment — they route to `bench-fallback` with a different rendered header noting the fork-PR origin.

### End-to-end flow examples

For a PR opened on the main repo, dedicated machine online:
1. `detect-runner` (on shared runner): ~30s — confirms dedicated is online
2. `bench-dedicated` (on dedicated): 10–25 min — runs scenario tier, computes diff vs main, posts sticky comment
3. Total: ~25 min from PR-event to comment

For the same PR with dedicated offline:
1. `detect-runner`: ~30s — confirms dedicated is offline
2. `bench-fallback` (on shared): ~10 min — runs scenario tier, computes diff vs main, posts sticky comment with offline-warning header
3. Total: ~10 min, with degraded signal

For a fork PR (regardless of dedicated state):
1. `detect-runner`: ~30s
2. `bench-fallback`: ~10 min — fork-PR header in comment
3. Total: ~10 min

---

## Result transport

Three modes, three transport paths. Each starts from the same source (`bench/results/<UTC-ISO8601>/cross-engine.md`, `summary.md`, `results.json` produced by `chisel-bench-summarize`) and routes the artifacts to a mode-appropriate destination.

### Per-PR (mode b, Spec 3): Actions artifact + sticky comment

The existing PR 5 pattern, modified for the fallback case. Both `bench-dedicated` and `bench-fallback` upload a workflow artifact:

```yaml
- name: Upload PR-side bench artifacts
  if: always()
  uses: actions/upload-artifact@v4
  with:
    name: bench-results-pr-${{ github.event.pull_request.number }}
    path: /tmp/pr-out/
    if-no-files-found: warn

- name: Render diff and post sticky PR comment
  uses: peter-evans/create-or-update-comment@v4
  with:
    comment-id: ${{ steps.find-comment.outputs.comment-id }}
    issue-number: ${{ github.event.pull_request.number }}
    body-path: /tmp/diff-comment.md
    edit-mode: replace
```

The diff comment body is rendered by `chisel-bench-diff`. The new behavior in v1 of the dedicated machine: the fallback job prepends a header before posting:

For an offline-machine fallback:
```markdown
> ⚠️ **Ran on shared GitHub-hosted runner** — dedicated bench machine is offline. Numbers have ±15% variance; treat the diff signal as noisy. Re-trigger after the machine is back online for trustworthy values.
```

For a fork-PR fallback:
```markdown
> ℹ️ **Fork PR — ran on shared GitHub-hosted runner** — fork PRs do not run on the dedicated bench machine for security reasons. Numbers have ±15% variance.
```

Same sticky comment marker (`<!-- chisel-bench-diff -->`) in both cases so subsequent pushes update in place. The dedicated-runner version posts no header — the absence of a warning **is** the signal that the comment is trustworthy.

Auth: `${{ secrets.GITHUB_TOKEN }}` with `permissions: pull-requests: write`. No new secrets.

### Canonical (mode a, Spec 2): GitHub Release attachment

Triggered on `release: types: [published]`. The workflow uploads the rendered artifacts as Release assets:

```yaml
- name: Upload canonical bench results to release
  env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: |
    gh release upload "${{ github.event.release.tag_name }}" \
      bench/results/<UTC>/cross-engine.md \
      bench/results/<UTC>/summary.md \
      bench/results/<UTC>/results.json \
      --clobber
```

`--clobber` allows re-runs (via `workflow_dispatch:`) to overwrite earlier uploads.

The README is updated manually as part of the release-cutting checklist to link to the latest release's `cross-engine.md`. Not in this spec — that's a one-line README change documented in the release-cutting runbook.

Auth: `${{ secrets.GITHUB_TOKEN }}` with `permissions: contents: write`. No PAT.

Failure modes:
- **Bench fails before reaching the upload step:** workflow fails; release exists without canonical bench attachments; operator re-runs via `workflow_dispatch:` after fixing.
- **Upload step fails (network, permission):** workflow fails; same recovery.

### Soak (mode d, Spec 4): committed to main on dated path

Soak's outputs commit directly to `main` under `bench-results/soak/<UTC-ISO8601>/`:

```yaml
- name: Commit soak results to main
  env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: |
    UTC=$(date -u +%Y-%m-%dT%H-%M-%SZ)
    DEST="bench-results/soak/${UTC}"
    mkdir -p "${DEST}"
    cp bench/results/<run-UTC>/cross-engine.md "${DEST}/"
    cp bench/results/<run-UTC>/summary.md "${DEST}/"
    cp bench/results/<run-UTC>/results.json "${DEST}/"
    cp bench/results/<run-UTC>/soak-metrics.md "${DEST}/" || true
    git config user.name "chisel-bench-bot"
    git config user.email "chisel-bench-bot@users.noreply.github.com"
    git add "${DEST}"
    git commit -m "soak: bench run ${UTC}" \
               -m "Automated soak benchmark run on dedicated bench machine."
    git push origin main
```

Auth: `${{ secrets.GITHUB_TOKEN }}` with `permissions: contents: write`.

Identity: a dedicated machine identity `chisel-bench-bot` keeps automated commits distinguishable from human contributions in `git log`. The `users.noreply.github.com` email is a GitHub convention that doesn't require a real account; commits are attributed to "anonymous bot" in the GitHub UI.

Failure modes:
- **Push fails because main moved between fetch and push** (someone merged a PR during the soak window): retry once with `git pull --rebase origin main && git push`. Soak's commits are isolated to `bench-results/soak/<UTC>/`, so there is no merge conflict to worry about. If retry also fails, abort and emit an alert artifact.
- **Branch protection on main blocks bot commits:** surfaces at first soak run. Operator decides whether to allow `chisel-bench-bot` in the protection bypass list, or relax the protection rule. v1 default: assume no branch protection (Xof/chisel currently has none); design preserves the option to switch to a `bench-results-soak` orphan branch later if protection becomes a problem.
- **Bench itself fails:** workflow fails; no commit happens; the next scheduled run tries again. Operator triages via Actions UI failure notification.

### Cross-cutting: what travels vs what stays local

Three text artifacts always travel: `cross-engine.md`, `summary.md`, `results.json`. Soak adds `soak-metrics.md` (defined in Spec 4). **`raw/` (Criterion archives) is never committed or uploaded for any mode.** Storage stays bounded: per-PR artifacts are ~5 KB each; Release assets are ~5 KB per release; soak commits add ~5 KB per week (~250 KB/year, negligible).

The `raw/` directory remains on the dedicated machine's local disk for ad-hoc post-hoc analysis (operator SSH plus inspection). The disk-management workflow prunes it on the 14-day cadence.

---

## Failure modes and operational basics

### Disk management

Local disk usage grows from three sources:
- `~/actions-runner/_work/<repo>/<repo>/target/` — cargo build artifacts (incremental compile cache; ~2–5 GB after sustained use)
- `~/actions-runner/_work/<repo>/<repo>/bench/results/<UTC>/` — accumulated bench output directories (~5 MB per run including `raw/`)
- `/var/log/` and `/var/cache/apt/` — system logs and package cache

`bench-disk-cleanup.yml` (nightly, on the dedicated runner):
1. Prunes `bench/results/<UTC>/` directories older than 14 days.
2. Runs `cargo clean` on the bench subcrate **only if** free disk drops below 30 GiB. Cargo rebuild from scratch takes ~3–5 minutes; the incremental cache is worth keeping when there is room.
3. Runs `sudo apt-get clean` to drop the package cache.
4. Emits a tiny `disk-status.txt` artifact with `df -h /` output for forensic visibility.

**Hard backstop:** every bench workflow has a pre-job step that checks `df -h /` and refuses to start the run if free disk is below 20 GiB, posting an operator alert as a workflow annotation. This prevents a bench run from filling the disk mid-execution and corrupting state.

### Log retention

| Source | Where it lives | Retention |
|---|---|---|
| Actions workflow logs (per-job stdout/stderr) | GitHub Actions UI | 90 days (GitHub default) |
| Per-PR sticky comments | GitHub PR thread | Forever |
| Canonical Release assets | GitHub Releases page | Forever |
| Soak commits | git history on `main` | Forever |
| Machine-side syslog (runner agent, system) | `/var/log/syslog`, journald | 7 days (logrotate default) |
| Bench output directories on disk | `bench/results/<UTC>/` | 14 days (pruned by `bench-disk-cleanup.yml`) |

No log shipping to external services in v1. If machine-side logs become forensically important (correlating an Actions failure with system load), the operator SSHs in within the 7-day journald window. Future enhancement if outages stop being rare: ship journald to an external aggregator.

### OS upgrades

`unattended-upgrades` is configured at provisioning time:
- `Unattended-Upgrade::Allowed-Origins`: security only (not regular package updates)
- `Unattended-Upgrade::Automatic-Reboot`: false (don't reboot autonomously — would kill in-flight bench runs)

`bench-os-update.yml` (monthly, on the dedicated runner) runs `sudo unattended-upgrade` synchronously and reboots the machine **only if** the runner queue is empty at the time. Kernel patches land but stay inactive between monthly runs — acceptable since the machine is not internet-facing for arbitrary inbound.

### Runner agent updates

GitHub's runner agent auto-updates by default between jobs. The `--disableupdate` flag is an option but not used in v1 — auto-update keeps the agent on a supported version, and the between-jobs window is small enough that update-induced disruption is rare. If a runner update breaks something, the rollback path is to re-register the runner with a pinned version.

### Machine-level failures and recovery

| Failure | Symptom | Recovery |
|---|---|---|
| VM crashes / hard reboot | Actions UI shows running job as failed; runner agent re-registers on boot | In-flight job is lost; trigger source (PR push, schedule, release) re-runs on next event. Operator SSHs in to verify clean state. |
| Runner agent crashes | Actions UI shows runner offline; no jobs queue | systemd auto-restarts the runner (`Restart=always`); if persistent, operator runs `sudo journalctl -u actions.runner.* -n 200` |
| Network drops | Workflow fails with curl/connection errors | Built-in Actions retry on transient failures; operator-visible if persistent |
| OOM kill | Workflow step fails with exit 137 | Surfaces in Actions UI; investigate via journalctl; v1 mitigation is the 8 GiB minimum. Soak workloads (Spec 4) are the most likely to trigger OOM and will define memory bounds. |
| Disk full mid-run | Bench step fails with ENOSPC | Pre-job check prevents this when free disk < 20 GiB; manual cleanup via SSH if it slips through. |
| Cloud-provider drift | Subtle: noise gate may start failing | Post-v1 noise-monitor workflow catches drift; v1 catches via "PR diff comments suddenly noisier" — operator manually re-runs the noise gate. |

### Maintenance windows

For OS upgrades, runner updates, hardware changes:

```bash
ssh operator@dedicated-bench
sudo systemctl stop actions.runner.<repo>.<runner-name>
# Wait for in-flight job to complete (visible in Actions UI)
# ... do maintenance ...
sudo systemctl start actions.runner.<repo>.<runner-name>
```

While stopped, per-PR work falls back to the shared runner (with the offline-warning header); canonical and soak runs queue in Actions UI until the runner returns. No external coordination needed.

### Observability

Three signals, no external tooling in v1:

1. **GitHub Actions failure notifications** (email + GitHub web): operator receives an email on any workflow run failure. Already enabled if the operator has notifications on for the repo.
2. **Daily disk-status artifact** from `bench-disk-cleanup.yml`: operator can `gh run download` the latest artifact to see disk trend without SSHing in.
3. **Per-PR comment cadence:** when the dedicated machine is offline, every PR's bench comment carries the offline-warning header. Operator notices in normal PR-review flow.

For v1, deemed sufficient. Future: Prometheus exporter, health-check artifact uploads, etc. — each has cost (auth setup, dashboard maintenance) and v1 doesn't justify it.

### SSH access

Operator (you) holds SSH access via public key configured at provisioning time. No other accounts. SSH is the only inbound network surface; everything else (runner agent, git, etc.) is outbound-only. Hardening: `PasswordAuthentication no`, `PermitRootLogin no`, `fail2ban` enabled.

A second backup public key (operator-controlled, stored in a separate location) is configured at provisioning for emergency access. v1 does not include team-shared access — single-operator model matches the single-author repo.

---

## Build sequence

Eight steps, each independently testable. Sequenced so each step's success criterion is observable before the next begins.

### Step 1: Provider plus VM provisioning (~1 hour wall clock; expect to repeat if noise gate later fails)

- Pick a provider from the shortlist; provision a VM matching the sizing minimums.
- Configure SSH keys (primary plus backup), `PasswordAuthentication no`, `PermitRootLogin no`, `fail2ban`, `unattended-upgrades` for security-only with `Automatic-Reboot: false`.
- Set hostname, timezone (UTC), basic sysctl tuning if any.
- **Success:** SSH from operator's machine works; `df -h` shows expected disk; `apt list --installed` shows expected baseline.

### Step 2: Toolchain plus repo clone

- Install `rustup` with stable toolchain.
- Install `git`, `gh`, `build-essential`, anything else the bench subcrate needs.
- Clone `Xof/chisel`; `cd bench && cargo build --release` to warm the cargo cache.
- **Success:** `cd bench && cargo test` passes (98/98 tests).

### Step 3: Self-hosted runner installation plus registration

- Download GitHub's runner agent for Linux x64 from the release page.
- Register: `./config.sh --url https://github.com/Xof/chisel --token <runner-token>`. Token from repo settings → Actions → Runners → New self-hosted runner.
- Add labels: `dedicated-bench`, `bench-v1` (in addition to default `self-hosted`, `linux`, `x64`).
- Install as systemd service: `sudo ./svc.sh install && sudo ./svc.sh start`.
- **Success:** Repo settings shows the runner online; `sudo systemctl status actions.runner.*` shows active.

### Step 4: Noise-validation gate (~50 minutes wall clock for 5 runs)

- Run the gate script by hand: 5 back-to-back `cargo bench --bench scenarios` invocations, then COV computation.
- If COV exceeds thresholds, tear down the VM and try a different instance type or provider — start over from Step 1.
- **Success:** `noise-gate-report.md` shows all cells under threshold; commit the report into the repo at `bench-results/noise-gate/<UTC>-<provider>-<instance-type>.md`.

### Step 5: Workflow scaffolding for per-PR mode (the `bench.yml` migration)

- Edit `bench.yml`: add the `detect-runner` job plus `bench-dedicated` (with fork-PR gate) plus `bench-fallback` (with header rendering).
- Update the diff-comment rendering in the bench-side code (`bench/src/bin/diff.rs` or a separate render module) to support the warning-header prefix.
- Open a test PR against `main` — confirm the workflow routes to dedicated, runs, posts a comment without warning header.
- Force the runner offline (`sudo systemctl stop actions.runner.*`); open another test PR; confirm the workflow routes to fallback and posts a comment with the offline-warning header.
- Re-start the runner.
- **Success:** Both code paths produce the expected sticky comment; the operator notices nothing different about per-PR feedback when the dedicated machine is online.

### Step 6: Disk-cleanup workflow

- Add `bench-disk-cleanup.yml` (nightly schedule plus workflow_dispatch).
- Run it once via `gh workflow run bench-disk-cleanup.yml`.
- Verify `disk-status.txt` artifact shows expected df output; verify old `bench/results/<UTC>/` directories were pruned (if any existed).
- **Success:** Workflow completes; operator can `gh run download` the artifact to see disk state.

### Step 7: OS-update workflow

- Add `bench-os-update.yml` (monthly schedule plus workflow_dispatch).
- Run it once via `gh workflow run bench-os-update.yml`.
- Verify it ran the security upgrade synchronously; verify it didn't reboot (since the queue wasn't empty at trigger time, or because no kernel patch was pending).
- **Success:** Workflow completes; `apt list --upgradable` on the machine shows nothing (or only non-security packages).

### Step 8: Operator runbook

- Write `docs/operations/dedicated-bench-runbook.md` covering: how to drain the runner for maintenance, how to re-run the noise gate, how to roll back a runner update, how to reproduce a per-PR run by hand, how to triage a soak failure (when Spec 4 ships).
- Documentation, not infrastructure — the existence of the runbook is the deliverable.
- **Success:** Runbook reviewed by operator (you), committed to repo.

### Foundation v1 ships when

All 8 steps complete AND at least one of Specs 2/3/4 is implemented end to end on top. The "at least one consumer" criterion guards against shipping infrastructure that no real workflow uses.

Most likely first consumer: **Spec 3** (per-PR low-noise regression detection) because Step 5 already implements most of it. Remaining work for Spec 3 is mostly the diff-rendering enhancements and the documented operator runbook.

---

## macOS extensibility (preserved, not implemented)

The design preserves the ability to add a second machine for macOS coverage (mode c) without re-architecting:

- The runner-label scheme (`bench-v1`, eventually `bench-macos-v1`) supports multi-machine fan-out without workflow rewrites.
- The workflow scaffolding pattern (one `bench-*.yml` per mode, `runs-on:` selects machine) extends naturally — add a `runs-on: [self-hosted, macos, dedicated-bench, bench-macos-v1]` matrix entry to the same workflow file or fork into a `bench-macos-*.yml` family.
- The result-transport mechanisms (Releases, main commit, Actions artifact) all already work on macOS — `gh`, `git`, and `actions/upload-artifact` are platform-agnostic.
- The per-PR diff binary would need a "two-machine cross-platform" rendering mode (per-platform tables side by side, or a platform column added to existing tables); not in this spec, not in Spec 3, but the data flow supports it.

The macOS machine would also need its own noise-gate qualification at provisioning time, with thresholds that may differ from Linux's (`F_FULLFSYNC` makes the noise floor different).

---

## Rejected alternatives

What we considered and why we didn't pick it. Useful for future readers wondering "why didn't we just…":

- **Bare-metal hardware** — would give better noise floor, but high upfront cost (rack space, hardware lifecycle) for a project that fits comfortably on a cloud VM. Reconsider if v1 cloud-VM noise gate keeps failing across providers.
- **On-demand spin-up per run** — would cut cost, but cold disk on every run zeroes `cache_hits` counter and materially changes ChiselEngine throughput on read-heavy workloads. The bench measures cache behavior; cold-starting it throws away the measurement.
- **Separate Linux cron** for canonical/soak — operationally simpler from a "no GitHub Actions involvement" angle, but introduces a second auth model (PAT or deploy key for git push, separate token for Releases) and a second log destination (machine-side syslog vs. Actions UI). Actions schedule running on the self-hosted runner gives the same cron-style behavior with one mechanism.
- **Pre-emption-based concurrency** (per-PR jobs preempt soak) — possible, complex, premature for v1 when we don't yet know how often per-PR jobs will clash with soak. Strict serial queue is simple; revisit if clash becomes a real pain point.
- **GitHub Pages dashboard for results** — pretty, useful for browsing trends, but a separate site to maintain. Out of scope for v1; revisit if results consumption patterns suggest it.
- **Replace `bench.yml` with no fallback (fail loud)** — philosophically purist but worse UX: PRs lose their bench comment for the duration of any machine outage. Falling back to the shared runner with a warning header keeps the signal flowing.
- **`pull_request_target` with required-reviewer environments** for fork PR safety — solves a different problem (running with elevated privileges on PR-content commits) and adds operator overhead per fork PR. The cheap `if:` gate that skips dedicated-runner work for fork PRs is sufficient for a single-author repo.

---

## Implementation plan handoff

Plan filename: `docs/plans/2026-05-04-dedicated-bench-machine-foundation.md`. The plan will decompose the 8 build steps above into per-step tasks with verification commands, expected outputs, and commit-message templates.

Plan delivery follows after this spec is reviewed and approved.
