# Dedicated Benchmark Machine — Operator Runbook

Companion document to [`docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md`](../specs/2026-05-04-dedicated-bench-machine-foundation-design.md). The spec describes what the system is and why; this runbook describes what the operator does to keep it running.

## Machine inventory

- **Hostname:** `chisel-bench-1`
- **Provider:** (record at deployment time, e.g., Hetzner CCX23)
- **Location:** (record at deployment time, e.g., Falkenstein DC)
- **Public IP:** (record at deployment time)
- **Operator account:** `bench-op` (SSH-key-only auth)
- **GitHub Actions runner labels:** `self-hosted`, `linux`, `x64`, `dedicated-bench`, `bench-v1`

## Common procedures

### Drain the runner for maintenance

When you need to stop the runner cleanly so an in-flight job completes before maintenance starts:

```bash
ssh bench-op@chisel-bench-1
# Confirm what's running first:
sudo systemctl list-units 'actions.runner.*' --type=service
# Stop the service (waits for current job to complete naturally):
sudo systemctl stop actions.runner.Xof-chisel.chisel-bench-1.service
```

While stopped: per-PR work falls through to `ubuntu-latest` automatically (with the offline-warning header on the PR comment). Canonical and soak runs queue in the GitHub Actions UI until the runner returns. After maintenance:

```bash
sudo systemctl start actions.runner.Xof-chisel.chisel-bench-1.service
sudo systemctl status actions.runner.Xof-chisel.chisel-bench-1.service
```

### Re-run the noise gate

After any environmental change (instance-type swap, provider migration, OS reinstall, kernel upgrade you suspect changed scheduler behavior):

```bash
ssh bench-op@chisel-bench-1
cd ~/work/chisel
git pull
cd bench
cargo build --release --bin chisel-bench-noise-gate
./target/release/chisel-bench-noise-gate \
  --provider <provider> \
  --instance-type <instance> \
  --runs 5 \
  --out /tmp/noise-gate-report.md
cat /tmp/noise-gate-report.md
```

If the report shows PASS, copy it back to your workstation and commit to `bench-results/noise-gate/<UTC>-<provider>-<instance>.md`.

If the report shows FAIL, you need to either change instance type / provider (re-run setup from Phase 1) or accept that the machine no longer qualifies for production use until the issue is resolved.

### Reproduce a per-PR bench run by hand

To debug a per-PR diff result, run the same workflow steps the dedicated-runner workflow runs:

```bash
ssh bench-op@chisel-bench-1
cd ~/work/chisel
git fetch origin pull/<PR-NUMBER>/head:debug-pr
git checkout debug-pr
cd bench
# Note: the summarize binary is registered as `summarize` (not
# `chisel-bench-summarize`) for historical reasons — see bench/Cargo.toml.
# The diff and noise-gate binaries use the long `chisel-bench-*` form.
cargo build --release --bench scenarios --bin summarize --bin chisel-bench-diff
cargo bench --bench scenarios
# Paths below are relative to the current directory (bench/), so just
# `results/...` rather than `bench/results/...`.
cargo run --release --bin summarize -- \
  --scenarios results/scenarios_metrics.jsonl \
  --out /tmp/pr-out
# Switch to main and rerun:
git checkout main && git pull
cargo bench --bench scenarios
cargo run --release --bin summarize -- \
  --scenarios results/scenarios_metrics.jsonl \
  --out /tmp/main-out
# Diff:
cargo run --release --bin chisel-bench-diff -- \
  --baseline /tmp/main-out/results.json \
  --pr /tmp/pr-out/results.json \
  --out /tmp/diff.md
cat /tmp/diff.md
```

### Roll back a runner-agent update

If GitHub's runner agent auto-updated and broke something:

```bash
ssh bench-op@chisel-bench-1
cd ~/actions-runner
sudo ./svc.sh stop
sudo ./svc.sh uninstall

# Re-download a known-good version (replace 2.319.1 with the version that worked):
rm -rf bin/ externals/
curl -o actions-runner-linux-x64-2.319.1.tar.gz -L \
  https://github.com/actions/runner/releases/download/v2.319.1/actions-runner-linux-x64-2.319.1.tar.gz
tar xzf actions-runner-linux-x64-2.319.1.tar.gz

# Re-register with --disableupdate so it stays pinned:
./config.sh remove --token <REMOVE-TOKEN>
./config.sh \
  --url https://github.com/Xof/chisel \
  --token <REGISTER-TOKEN> \
  --name chisel-bench-1 \
  --labels dedicated-bench,bench-v1 \
  --disableupdate \
  --unattended

sudo ./svc.sh install bench-op
sudo ./svc.sh start
```

Get the REMOVE/REGISTER tokens from `https://github.com/Xof/chisel/settings/actions/runners`.

### Triage a soak failure (when Spec 4 ships)

Soak workflows (`bench-soak.yml`) write results to `bench-results/soak/<UTC>/`. If a scheduled soak run fails:

1. Check the Actions UI for the failure step and stderr.
2. SSH to the VM; look at `~/actions-runner/_diag/Worker_<run-id>.log` for the runner-side perspective.
3. Check `dmesg | tail -100` for OOM kills or kernel events.
4. Check `df -h /` — if disk filled up mid-run, the disk-cleanup workflow's hard-backstop should have prevented this; investigate why it didn't.

(Detailed soak-failure procedures will be added when Spec 4's workloads are implemented.)

## Emergency procedures

### Operator's primary SSH key is lost

Use the backup key configured in Phase 1, Task 1.2 Step 2. SSH from the second machine that holds it. Generate a new primary key, add it to `/home/bench-op/.ssh/authorized_keys`, remove the lost key.

### Runner is online but jobs stay queued forever

Concurrency group might be wedged. From the operator workstation:

```bash
gh run list --status in_progress --json databaseId,name,startedAt
# Identify the wedged run; cancel it:
gh run cancel <RUN-ID>
```

If the runner itself is stuck, restart the service on the VM:

```bash
ssh bench-op@chisel-bench-1
sudo systemctl restart actions.runner.Xof-chisel.chisel-bench-1.service
```

### VM is unresponsive (SSH hangs)

Hard reboot via cloud-provider console (Hetzner: Console → Server → Power → Reset; AWS: EC2 Console → Instance State → Reboot). After ~2 minutes, SSH should work again. The runner systemd service is `Restart=always`, so it'll be back online on its own.

If the VM doesn't come back from reboot, attach the cloud-provider rescue console to investigate filesystem corruption.
