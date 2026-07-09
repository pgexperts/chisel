# Dedicated Benchmark Machine Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a single always-on dedicated Linux cloud VM that serves as the canonical home for Chisel benchmark runs, with self-hosted Actions runner integration, a setup-time noise-validation gate, and operations workflows.

**Architecture:** Provisioned-IOPS cloud VM running Ubuntu 24.04 LTS, qualified by a 5-back-to-back-runs noise gate (COV ≤ 2% throughput / ≤ 5% p99). Single self-hosted GitHub Actions runner registered with `dedicated-bench` and `bench-v1` labels handles all bench workflows. Per-PR work falls back transparently to `ubuntu-latest` when the dedicated machine is offline. Companion ops workflows (disk cleanup, OS upgrade) run on the same runner.

**Tech Stack:** Ubuntu 24.04 LTS, GitHub Actions self-hosted runner agent, Rust (for the noise-gate binary in `bench/`), GitHub Actions YAML, systemd, `unattended-upgrades`, `gh` CLI.

**Reference spec:** `docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md`

---

## File Structure

This plan modifies or creates the following files:

**New code (Rust):**
- `bench/src/bin/noise_gate.rs` — `chisel-bench-noise-gate` binary that runs the gate
- `bench/src/noise_gate/mod.rs` — library module for noise-gate logic (COV computation, report rendering)
- `bench/src/noise_gate/cov.rs` — coefficient-of-variation computation, pure function, easy to unit-test
- `bench/src/noise_gate/report.rs` — markdown report rendering
- `bench/src/lib.rs` — add `pub mod noise_gate;`

**New code (workflow YAML):**
- `.github/workflows/bench-disk-cleanup.yml` — nightly disk pruning + `cargo clean` if low
- `.github/workflows/bench-os-update.yml` — monthly security upgrades + conditional reboot

**Modified code (workflow YAML):**
- `.github/workflows/bench.yml` — add `detect-runner` + `bench-dedicated` + `bench-fallback` jobs; replaces the existing single `bench` job

**Modified code (Rust):**
- `bench/src/bin/diff.rs` — add `--prepend-header` CLI option for the warning-header rendering

**New documentation:**
- `docs/operations/dedicated-bench-runbook.md` — operator runbook

**No changes to:**
- Root `chisel` crate
- `python/` subcrate
- `bench/src/summary/` (existing modules)
- `bench/src/bin/summarize.rs`

The plan also drives operator procedures (provisioning, OS config, runner installation) that don't touch the repo. Those procedures are captured as plan tasks but their "deliverable" is observable machine state, not committed files.

---

## Phase 1: VM provisioning and OS setup

**Operator procedures only — no code changes in this phase. Tasks produce observable machine state.**

### Task 1.1: Pick provider and provision the VM

**Files:** None (operator action)

- [ ] **Step 1: Pick a provider from the spec shortlist**

Reference: `docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md` § "Provider candidates worth measuring".

Recommended starting point: **Hetzner CCX23** (4 dedicated vCPUs, 16 GiB RAM, 160 GiB NVMe, ~€30/mo) for cost-effectiveness with NVMe local storage. Falls back to AWS `m7i.xlarge` with `gp3` (provisioned 16K IOPS) if Hetzner doesn't pass the noise gate.

- [ ] **Step 2: Provision the VM with the chosen instance type**

For Hetzner: Console → Add Server → Location: closest geographic region → Image: Ubuntu 24.04 → Type: CCX23 → Add SSH key (operator's primary public key).

For AWS: EC2 Console → Launch Instance → AMI: Ubuntu Server 24.04 LTS → Instance type: m7i.xlarge → Storage: 100 GiB gp3 with 16K IOPS provisioned → Security group: SSH from operator IP only.

Record the public IP / hostname for use in subsequent tasks.

- [ ] **Step 3: Verify SSH access**

Run from operator's workstation:

```bash
ssh ubuntu@<public-ip>      # or 'root@' for Hetzner
```

Expected: shell prompt opens. If not, check security-group / firewall rules.

### Task 1.2: Configure the operator account

**Files:** None (operator action on the VM)

- [ ] **Step 1: Create a dedicated operator account**

The cloud-provider default account is often `ubuntu` or `root`. Create a `bench-op` account for clarity:

```bash
sudo adduser --disabled-password --gecos "" bench-op
sudo usermod -aG sudo bench-op
sudo mkdir -p /home/bench-op/.ssh
sudo cp ~/.ssh/authorized_keys /home/bench-op/.ssh/
sudo chown -R bench-op:bench-op /home/bench-op/.ssh
sudo chmod 700 /home/bench-op/.ssh
sudo chmod 600 /home/bench-op/.ssh/authorized_keys
```

- [ ] **Step 2: Add the backup SSH key**

Operator's secondary key (stored separately from the primary) goes here for emergency access:

```bash
sudo bash -c 'echo "<backup-public-key-contents>" >> /home/bench-op/.ssh/authorized_keys'
```

- [ ] **Step 3: Verify backup key works**

From the second machine that holds the backup key:

```bash
ssh bench-op@<public-ip>
```

Expected: shell opens. If not, check key file permissions.

### Task 1.3: Harden SSH

**Files:** `/etc/ssh/sshd_config` on the VM (operator action)

- [ ] **Step 1: Edit SSH daemon config**

Run on the VM:

```bash
sudo sed -i 's/^#*PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
sudo sed -i 's/^#*PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sudo sed -i 's/^#*PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config
```

- [ ] **Step 2: Reload sshd**

```bash
sudo systemctl reload sshd
```

- [ ] **Step 3: Verify password authentication is rejected**

From the operator workstation:

```bash
ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no bench-op@<public-ip>
```

Expected: `Permission denied (publickey)`. If it prompts for a password, reload didn't take effect.

### Task 1.4: Install fail2ban

**Files:** `/etc/fail2ban/jail.local` on the VM

- [ ] **Step 1: Install fail2ban package**

```bash
sudo apt-get update && sudo apt-get install -y fail2ban
```

- [ ] **Step 2: Configure SSH jail**

Create `/etc/fail2ban/jail.local`:

```ini
[sshd]
enabled = true
port = ssh
maxretry = 5
bantime = 1h
```

- [ ] **Step 3: Restart fail2ban**

```bash
sudo systemctl restart fail2ban
sudo systemctl status fail2ban
```

Expected: `active (running)`.

### Task 1.5: Configure unattended-upgrades

**Files:** `/etc/apt/apt.conf.d/50unattended-upgrades`, `/etc/apt/apt.conf.d/20auto-upgrades` on the VM

- [ ] **Step 1: Install package (if not already installed)**

```bash
sudo apt-get install -y unattended-upgrades
```

- [ ] **Step 2: Edit `/etc/apt/apt.conf.d/50unattended-upgrades`**

Ensure the `Unattended-Upgrade::Allowed-Origins` block contains ONLY security:

```
Unattended-Upgrade::Allowed-Origins {
    "${distro_id}:${distro_codename}-security";
    "${distro_id}ESMApps:${distro_codename}-apps-security";
    "${distro_id}ESM:${distro_codename}-infra-security";
};
```

Set automatic reboot to **false** (we control reboots via the monthly OS-update workflow):

```
Unattended-Upgrade::Automatic-Reboot "false";
```

- [ ] **Step 3: Edit `/etc/apt/apt.conf.d/20auto-upgrades`**

Enable daily security checks:

```
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
```

- [ ] **Step 4: Verify config**

```bash
sudo unattended-upgrades --dry-run --debug 2>&1 | head -20
```

Expected: no errors; output shows allowed-origins matching the intended set.

### Task 1.6: Set hostname and timezone

**Files:** `/etc/hostname` on the VM (via `hostnamectl`)

- [ ] **Step 1: Set hostname**

```bash
sudo hostnamectl set-hostname chisel-bench-1
```

- [ ] **Step 2: Set timezone to UTC** (matches Actions schedule semantics)

```bash
sudo timedatectl set-timezone UTC
```

- [ ] **Step 3: Verify**

```bash
hostnamectl
timedatectl
```

Expected: hostname `chisel-bench-1`; time zone `Etc/UTC`.

### Task 1.7: Phase 1 success check

**Files:** None

- [ ] **Step 1: Verify operator-side state matches the spec's success criteria**

```bash
ssh bench-op@chisel-bench-1
# In the SSH session:
df -h /
apt list --installed 2>/dev/null | grep -E '(fail2ban|unattended-upgrades|openssh)' | head
hostnamectl
```

Expected: SSH works without password; disk shows expected size; fail2ban / unattended-upgrades installed; hostname set.

No commit at the end of Phase 1 — no code changes were made in the repo.

---

## Phase 2: Toolchain installation and repo clone

### Task 2.1: Install Rust toolchain

**Files:** None (operator action on the VM)

- [ ] **Step 1: SSH to the VM**

```bash
ssh bench-op@chisel-bench-1
```

- [ ] **Step 2: Install rustup**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"
```

- [ ] **Step 3: Verify toolchain**

```bash
rustc --version
cargo --version
```

Expected: stable Rust (≥ 1.75 or whatever ships with current stable channel).

### Task 2.2: Install build dependencies

**Files:** None (operator action)

- [ ] **Step 1: Install required system packages**

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev git curl jq
```

- [ ] **Step 2: Install GitHub CLI**

```bash
(type -p wget >/dev/null || (sudo apt update && sudo apt-get install wget -y)) \
  && sudo mkdir -p -m 755 /etc/apt/keyrings \
  && wget -qO- https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo tee /etc/apt/keyrings/githubcli-archive-keyring.gpg > /dev/null \
  && sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
  && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
  && sudo apt update \
  && sudo apt install gh -y
```

- [ ] **Step 3: Verify**

```bash
git --version
gh --version
jq --version
```

Expected: all three commands return version strings.

### Task 2.3: Clone the repo and warm the cargo cache

**Files:** None (operator action)

- [ ] **Step 1: Clone the repo**

```bash
mkdir -p ~/work && cd ~/work
git clone https://github.com/pgexperts/chisel.git
cd chisel
```

- [ ] **Step 2: Build the bench subcrate in release mode**

```bash
cd bench
cargo build --release
```

Expected: build completes without error; takes 3–5 minutes on first run as cargo downloads + compiles dependencies.

- [ ] **Step 3: Run bench tests as a smoke check**

```bash
cargo test
```

Expected: 98/98 tests pass. (This is the same test count we've been tracking in CLAUDE.md status updates.)

### Task 2.4: Phase 2 success check

- [ ] **Step 1: Confirm `cd bench && cargo test` passes**

Already done in Task 2.3 Step 3. Re-running here to make Phase 2's success criterion explicit.

```bash
cd ~/work/chisel/bench && cargo test 2>&1 | tail -5
```

Expected: `test result: ok. <N> passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`.

No repo commit yet — the repo on the VM is just a clone.

---

## Phase 3: Self-hosted Actions runner installation

### Task 3.1: Download and register the runner

**Files:** `~/actions-runner/` on the VM (operator action)

- [ ] **Step 1: Get the runner-registration token**

In a browser: navigate to `https://github.com/pgexperts/chisel/settings/actions/runners/new`. The page generates a one-time registration token (valid ~1 hour). Copy it.

- [ ] **Step 2: Download the runner agent on the VM**

Run on the VM (token from Step 1, version may need updating to current):

```bash
mkdir -p ~/actions-runner && cd ~/actions-runner
curl -o actions-runner-linux-x64-2.319.1.tar.gz -L \
  https://github.com/actions/runner/releases/download/v2.319.1/actions-runner-linux-x64-2.319.1.tar.gz
tar xzf ./actions-runner-linux-x64-2.319.1.tar.gz
```

- [ ] **Step 3: Register with the repo**

```bash
./config.sh \
  --url https://github.com/pgexperts/chisel \
  --token <TOKEN-FROM-STEP-1> \
  --name chisel-bench-1 \
  --labels dedicated-bench,bench-v1 \
  --unattended
```

The `--unattended` flag skips interactive prompts and uses defaults for runner group ("Default") and work folder ("`_work`").

- [ ] **Step 4: Verify the runner is registered**

In a browser: `https://github.com/pgexperts/chisel/settings/actions/runners` should now list `chisel-bench-1` with labels `self-hosted`, `linux`, `x64`, `dedicated-bench`, `bench-v1`. Status will be "offline" until Step 5.

### Task 3.2: Install the runner as a systemd service

**Files:** `/etc/systemd/system/actions.runner.pgexperts-chisel.chisel-bench-1.service` (created by `svc.sh install`)

- [ ] **Step 1: Install the service**

```bash
cd ~/actions-runner
sudo ./svc.sh install bench-op
```

`bench-op` is the user the service runs as.

- [ ] **Step 2: Start the service**

```bash
sudo ./svc.sh start
```

- [ ] **Step 3: Verify the service is running**

```bash
sudo ./svc.sh status
```

Expected: `Active: active (running)`.

- [ ] **Step 4: Verify the runner shows online in GitHub UI**

Refresh `https://github.com/pgexperts/chisel/settings/actions/runners` — `chisel-bench-1` should now show "Idle" (online and ready).

### Task 3.3: Phase 3 success check

- [ ] **Step 1: Trigger a smoke test workflow run on the runner**

This step will fail until Phase 5's `bench.yml` migration lands — defer to Task 5.7. For now, the success criterion is "runner shows online in GitHub UI and `sudo systemctl status actions.runner.*` shows active".

No repo commit yet — runner setup is operator-side only.

---

## Phase 4: Noise-gate binary (NEW CODE — TDD)

### Task 4.1: Add the noise_gate library skeleton

**Files:**
- Create: `bench/src/noise_gate/mod.rs`
- Create: `bench/src/noise_gate/cov.rs`
- Create: `bench/src/noise_gate/report.rs`
- Modify: `bench/src/lib.rs:1-5` (add `pub mod noise_gate;` line)

- [ ] **Step 1: Add module declaration to bench/src/lib.rs**

Find the existing `pub mod` declarations (alongside `summary`, `engines`, etc.) and add:

```rust
pub mod noise_gate;
```

If `bench/src/lib.rs` doesn't have the equivalent of `pub mod summary;`, place it consistently with the other public modules.

- [ ] **Step 2: Create bench/src/noise_gate/mod.rs**

```rust
// noise_gate.rs — Setup-time noise-validation gate for the dedicated
// benchmark machine.
//
// Runs the scenario tier N times back-to-back, computes coefficient of
// variation per cell across the runs, and reports whether the cells stay
// under threshold. Used at provisioning time to qualify a candidate
// instance type before it goes into production.
//
// Deliberately does NOT run continuously in production — periodic
// re-validation is deferred to a future bench-noise-monitor.yml workflow.

pub mod cov;
pub mod report;

pub use cov::{compute_cov, Cov};
pub use report::render_report;
```

- [ ] **Step 3: Create empty bench/src/noise_gate/cov.rs and report.rs**

`bench/src/noise_gate/cov.rs`:

```rust
// COV (coefficient of variation) computation: stddev / mean, expressed as
// a fraction (e.g., 0.02 = 2%). Used by the noise gate to score per-cell
// run-to-run variability across N runs.
```

`bench/src/noise_gate/report.rs`:

```rust
// Markdown rendering for the noise-gate report. Consumed by the
// noise_gate CLI binary; tested as a pure string-rendering function.
```

- [ ] **Step 4: Verify the crate still compiles**

```bash
cd bench && cargo build
```

Expected: clean build, possibly with a "module is empty" warning that we'll eliminate in the next task.

- [ ] **Step 5: Commit**

```bash
git add bench/src/lib.rs bench/src/noise_gate/
git commit -m "bench: add noise_gate module skeleton"
```

### Task 4.2: Write the failing COV test

**Files:**
- Modify: `bench/src/noise_gate/cov.rs` (add tests at bottom)

- [ ] **Step 1: Add the test (still no implementation)**

Append to `bench/src/noise_gate/cov.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Cov {
    pub mean: f64,
    pub stddev: f64,
    pub cov: f64, // stddev / mean, fraction (NaN if mean is 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cov_of_constant_series_is_zero() {
        let c = compute_cov(&[100.0, 100.0, 100.0, 100.0, 100.0]);
        assert_eq!(c.mean, 100.0);
        assert_eq!(c.stddev, 0.0);
        assert_eq!(c.cov, 0.0);
    }

    #[test]
    fn cov_of_known_series_matches_hand_calc() {
        // 5 values with mean = 100, sample stddev = sqrt(((-2)^2 + (-1)^2 + 0^2 + 1^2 + 2^2) / 4) = sqrt(2.5) ≈ 1.5811
        let c = compute_cov(&[98.0, 99.0, 100.0, 101.0, 102.0]);
        assert!((c.mean - 100.0).abs() < 1e-9, "mean wrong: {}", c.mean);
        assert!((c.stddev - 1.5811388300841898).abs() < 1e-9, "stddev wrong: {}", c.stddev);
        assert!((c.cov - 0.015811388300841896).abs() < 1e-9, "cov wrong: {}", c.cov);
    }

    #[test]
    fn cov_of_zero_mean_series_is_nan() {
        let c = compute_cov(&[0.0, 0.0, 0.0]);
        assert_eq!(c.mean, 0.0);
        assert_eq!(c.stddev, 0.0);
        assert!(c.cov.is_nan(), "cov of zero mean should be NaN, got {}", c.cov);
    }

    #[test]
    fn cov_of_single_sample_returns_zero_stddev() {
        // Sample stddev with N=1 is undefined (divide-by-zero on N-1).
        // Convention: return 0.0 stddev and 0.0 cov so the noise gate
        // can't false-fail on a one-run "series".
        let c = compute_cov(&[42.0]);
        assert_eq!(c.mean, 42.0);
        assert_eq!(c.stddev, 0.0);
        assert_eq!(c.cov, 0.0);
    }
}
```

- [ ] **Step 2: Add the function signature (no body — test should fail to compile)**

Append above the `#[cfg(test)]` block:

```rust
pub fn compute_cov(samples: &[f64]) -> Cov {
    todo!()
}
```

- [ ] **Step 3: Run the test, verify it fails**

```bash
cd bench && cargo test --lib noise_gate::cov::tests 2>&1 | tail -20
```

Expected: 4 tests panic with "not yet implemented" (the `todo!()` body).

### Task 4.3: Implement compute_cov

**Files:**
- Modify: `bench/src/noise_gate/cov.rs` (replace `todo!()` body)

- [ ] **Step 1: Implement compute_cov**

Replace the `todo!()` body with:

```rust
pub fn compute_cov(samples: &[f64]) -> Cov {
    let n = samples.len();
    if n == 0 {
        return Cov { mean: f64::NAN, stddev: 0.0, cov: f64::NAN };
    }
    let mean = samples.iter().sum::<f64>() / n as f64;
    if n == 1 {
        // Sample stddev with N=1 is undefined (divide-by-zero on N-1).
        // Return 0.0 stddev so the noise gate doesn't false-fail.
        return Cov { mean, stddev: 0.0, cov: 0.0 };
    }
    // Sample variance with Bessel's correction (divide by N-1, not N).
    // Bessel-corrected because we're treating the runs as a sample of
    // the underlying noise process, not as the entire population.
    let variance = samples.iter()
        .map(|s| (s - mean).powi(2))
        .sum::<f64>() / (n - 1) as f64;
    let stddev = variance.sqrt();
    let cov = if mean == 0.0 { f64::NAN } else { stddev / mean };
    Cov { mean, stddev, cov }
}
```

- [ ] **Step 2: Run the tests, verify they pass**

```bash
cd bench && cargo test --lib noise_gate::cov::tests 2>&1 | tail -10
```

Expected: 4/4 pass.

- [ ] **Step 3: Run full bench tests as regression check**

```bash
cd bench && cargo test 2>&1 | tail -5
```

Expected: 102/102 (98 existing + 4 new).

- [ ] **Step 4: Commit**

```bash
git add bench/src/noise_gate/cov.rs
git commit -m "bench: implement compute_cov for noise-gate qualification"
```

### Task 4.4: Add the report-rendering test

**Files:**
- Modify: `bench/src/noise_gate/report.rs`

- [ ] **Step 1: Add the report struct and a failing test**

Replace `bench/src/noise_gate/report.rs` content with:

```rust
// Markdown rendering for the noise-gate report. Consumed by the
// noise_gate CLI binary; tested as a pure string-rendering function.

use crate::noise_gate::cov::Cov;

#[derive(Debug, Clone)]
pub struct CellResult {
    pub scenario: String,
    pub engine: String,
    pub mode: String,
    pub throughput: Cov,
    pub p99_latency_ns: Cov,
    /// True if BOTH throughput.cov ≤ throughput_threshold AND
    /// p99_latency_ns.cov ≤ p99_threshold.
    pub passes: bool,
}

#[derive(Debug, Clone)]
pub struct GateResult {
    pub provider: String,         // e.g., "hetzner"
    pub instance_type: String,    // e.g., "ccx23"
    pub run_count: usize,         // e.g., 5
    pub throughput_threshold: f64, // e.g., 0.02 = 2%
    pub p99_threshold: f64,       // e.g., 0.05 = 5%
    pub cells: Vec<CellResult>,
}

impl GateResult {
    pub fn all_pass(&self) -> bool {
        self.cells.iter().all(|c| c.passes)
    }
}

pub fn render_report(result: &GateResult) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell(scenario: &str, engine: &str, throughput_cov: f64, p99_cov: f64, passes: bool) -> CellResult {
        CellResult {
            scenario: scenario.to_string(),
            engine: engine.to_string(),
            mode: "strict".to_string(),
            throughput: Cov { mean: 1000.0, stddev: 1000.0 * throughput_cov, cov: throughput_cov },
            p99_latency_ns: Cov { mean: 1_000_000.0, stddev: 1_000_000.0 * p99_cov, cov: p99_cov },
            passes,
        }
    }

    fn sample_result_pass() -> GateResult {
        GateResult {
            provider: "hetzner".to_string(),
            instance_type: "ccx23".to_string(),
            run_count: 5,
            throughput_threshold: 0.02,
            p99_threshold: 0.05,
            cells: vec![
                sample_cell("ycsb-a", "chisel", 0.005, 0.02, true),
                sample_cell("ycsb-a", "redb", 0.008, 0.03, true),
                sample_cell("ycsb-a", "sqlite", 0.012, 0.04, true),
            ],
        }
    }

    #[test]
    fn report_includes_provider_and_instance_in_header() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("hetzner"), "missing provider: {r}");
        assert!(r.contains("ccx23"), "missing instance type: {r}");
    }

    #[test]
    fn report_includes_pass_summary_when_all_pass() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("PASS"), "missing PASS marker: {r}");
        assert!(r.contains("3 / 3 cells"), "missing pass count: {r}");
    }

    #[test]
    fn report_includes_fail_summary_when_any_fails() {
        let mut result = sample_result_pass();
        result.cells[1].throughput.cov = 0.05; // exceeds 2% threshold
        result.cells[1].passes = false;
        let r = render_report(&result);
        assert!(r.contains("FAIL"), "missing FAIL marker: {r}");
        assert!(r.contains("2 / 3 cells"), "missing pass count: {r}");
    }

    #[test]
    fn report_includes_per_cell_table_with_cov_percentages() {
        let r = render_report(&sample_result_pass());
        assert!(r.contains("ycsb-a"), "missing scenario: {r}");
        assert!(r.contains("chisel"), "missing engine: {r}");
        // Throughput cov = 0.005 = 0.5%; should render as percentage with one decimal place.
        assert!(r.contains("0.5%"), "missing throughput cov as percentage: {r}");
    }

    #[test]
    fn report_failing_section_lists_failing_cells_with_thresholds() {
        // Without this test, the `if !result.all_pass()` branch in
        // render_report (the "Failing cells" detail section) is uncovered.
        let mut result = sample_result_pass();
        result.cells[0].throughput.cov = 0.05;
        result.cells[0].passes = false;
        let r = render_report(&result);
        assert!(r.contains("## Failing cells"), "missing failing cells section: {r}");
        assert!(r.contains("threshold 2.0%"), "missing threshold value: {r}");
    }
}
```

- [ ] **Step 2: Run, verify the tests fail**

```bash
cd bench && cargo test --lib noise_gate::report::tests 2>&1 | tail -10
```

Expected: 5 tests fail (panic with "not yet implemented").

### Task 4.5: Implement render_report

**Files:**
- Modify: `bench/src/noise_gate/report.rs` (replace `todo!()` body)

- [ ] **Step 1: Implement render_report**

Replace the `todo!()` body in `render_report` with:

```rust
pub fn render_report(result: &GateResult) -> String {
    let pass_count = result.cells.iter().filter(|c| c.passes).count();
    let total = result.cells.len();
    let verdict = if result.all_pass() { "PASS" } else { "FAIL" };

    let mut out = String::new();
    out.push_str("# Chisel Bench Noise-Gate Report\n\n");
    out.push_str(&format!(
        "**Verdict:** {verdict} ({pass_count} / {total} cells under threshold)\n\n"
    ));
    out.push_str(&format!("- Provider: `{}`\n", result.provider));
    out.push_str(&format!("- Instance type: `{}`\n", result.instance_type));
    out.push_str(&format!("- Run count: {}\n", result.run_count));
    out.push_str(&format!(
        "- Throughput threshold: ≤ {:.1}% COV\n",
        result.throughput_threshold * 100.0
    ));
    out.push_str(&format!(
        "- p99 latency threshold: ≤ {:.1}% COV\n\n",
        result.p99_threshold * 100.0
    ));

    out.push_str("## Per-cell results\n\n");
    out.push_str("| Scenario | Engine | Mode | Throughput COV | p99 COV | Verdict |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for cell in &result.cells {
        // ASCII PASS/FAIL (not Unicode ✓/✗) for terminal/email/CI viewer
        // consistency. Project convention: no emojis or Unicode glyphs in
        // rendered output.
        let verdict_marker = if cell.passes { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.1}% | {} |\n",
            cell.scenario,
            cell.engine,
            cell.mode,
            cell.throughput.cov * 100.0,
            cell.p99_latency_ns.cov * 100.0,
            verdict_marker,
        ));
    }
    out.push('\n');

    if !result.all_pass() {
        out.push_str("## Failing cells\n\n");
        for cell in result.cells.iter().filter(|c| !c.passes) {
            out.push_str(&format!(
                "- `{}` / `{}` / `{}`: throughput COV {:.1}% (threshold {:.1}%), p99 COV {:.1}% (threshold {:.1}%)\n",
                cell.scenario,
                cell.engine,
                cell.mode,
                cell.throughput.cov * 100.0,
                result.throughput_threshold * 100.0,
                cell.p99_latency_ns.cov * 100.0,
                result.p99_threshold * 100.0,
            ));
        }
    }

    out
}
```

- [ ] **Step 2: Run the tests, verify they pass**

```bash
cd bench && cargo test --lib noise_gate::report::tests 2>&1 | tail -10
```

Expected: 5/5 pass.

- [ ] **Step 3: Run full bench tests**

```bash
cd bench && cargo test 2>&1 | tail -5
```

Expected: 107/107 passing.

- [ ] **Step 4: Commit**

```bash
git add bench/src/noise_gate/report.rs
git commit -m "bench: implement noise-gate markdown report rendering"
```

### Task 4.6: Implement the noise_gate CLI binary

**Files:**
- Create: `bench/src/bin/noise_gate.rs`
- Modify: `bench/Cargo.toml` (add `[[bin]]` section if not auto-discovered)

- [ ] **Step 1: Add an explicit [[bin]] block to bench/Cargo.toml**

The bench subcrate uses explicit `[[bin]]` declarations (currently `name = "summarize"` and `name = "chisel-bench-diff"`). Auto-discovery would name the new binary `noise_gate` based on filename, but we want the long form for consistency with `chisel-bench-diff`. Add a new block:

```toml
[[bin]]
name = "chisel-bench-noise-gate"
path = "src/bin/noise_gate.rs"
```

(Existing-binary naming is inconsistent — `summarize` vs `chisel-bench-diff` — for historical reasons. Don't try to fix the inconsistency in this plan; that's a separate decision.)

- [ ] **Step 2: Create bench/src/bin/noise_gate.rs**

```rust
// CLI entry point for chisel-bench-noise-gate.
//
// Runs the scenario tier N times back-to-back and reports per-cell
// coefficient of variation. Used at provisioning time to qualify a
// candidate dedicated-bench machine before it goes into production.
//
// The orchestration logic (subprocess invocation of `cargo bench --bench
// scenarios` and parsing of bench/results/scenarios_metrics.jsonl) lives
// here; the COV computation and report rendering are in the
// chisel_bench::noise_gate library module.

// Cov is intentionally NOT imported — it's part of compute_cov's return
// type but never referenced by name in this binary, and clippy fails
// unused imports under -D warnings.
use chisel_bench::noise_gate::{compute_cov, render_report};
use chisel_bench::noise_gate::report::{CellResult, GateResult};
use clap::Parser;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(name = "chisel-bench-noise-gate", version)]
#[command(about = "Runs the scenario tier N times and reports per-cell COV")]
struct Cli {
    /// Provider name (e.g., "hetzner") — recorded in the report header.
    #[arg(long)]
    provider: String,

    /// Instance type (e.g., "ccx23") — recorded in the report header.
    #[arg(long)]
    instance_type: String,

    /// Number of back-to-back runs.
    #[arg(long, default_value = "5")]
    runs: usize,

    /// Throughput COV threshold (fraction; 0.02 = 2%).
    #[arg(long, default_value = "0.02")]
    throughput_threshold: f64,

    /// p99 latency COV threshold (fraction; 0.05 = 5%).
    #[arg(long, default_value = "0.05")]
    p99_threshold: f64,

    /// Output path for the markdown report.
    #[arg(long, default_value = "noise-gate-report.md")]
    out: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(passed) => {
            if passed {
                println!("Noise gate PASSED — see {}", cli.out.display());
                ExitCode::SUCCESS
            } else {
                eprintln!("Noise gate FAILED — see {}", cli.out.display());
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("Noise gate error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error>> {
    // Collect per-run cell metrics: keyed by (scenario, engine, mode).
    let mut samples: BTreeMap<(String, String, String), Vec<(f64, f64)>> = BTreeMap::new();

    for run_idx in 0..cli.runs {
        eprintln!("Run {} of {} ...", run_idx + 1, cli.runs);

        // Truncate scenarios_metrics.jsonl before each run so we read only
        // this run's results. Path uses env!("CARGO_MANIFEST_DIR") so it's
        // cwd-independent — bench/benches/scenarios.rs writes to the same
        // {manifest_dir}/results/ path regardless of where the binary is
        // invoked from.
        let metrics_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("results")
            .join("scenarios_metrics.jsonl");
        if metrics_path.exists() {
            fs::remove_file(&metrics_path)?;
        }

        // current_dir on the cargo subprocess is required because the
        // bench/ subcrate is NOT a workspace member of the root chisel
        // crate — `cargo bench --bench scenarios` only resolves from
        // bench/ itself.
        let status = Command::new("cargo")
            .args(["bench", "--bench", "scenarios"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()?;
        if !status.success() {
            return Err(format!("Run {} failed: cargo bench exited {}", run_idx + 1, status).into());
        }

        // Parse the JSONL written by this run.
        //
        // ScenarioResult (bench/src/runner.rs) emits {scenario, mode,
        // total_wall_clock_ns, p99_ns, ...}. There is NO separate `engine`
        // field — `mode` is the combined EngineMode::label() string like
        // "chisel-strict", "redb-strict", "sqlite-strict". Split on the
        // first hyphen to recover (engine, durability).
        //
        // All field reads use .ok_or(...) (not .unwrap_or(...)) so a
        // schema regression fails loudly rather than silently averaging
        // zeros into the cells.
        let contents = fs::read_to_string(&metrics_path)?;
        for (lineno, line) in contents.lines().enumerate() {
            let entry: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| format!("line {} in metrics jsonl: {}", lineno + 1, e))?;
            let scenario = entry["scenario"]
                .as_str()
                .ok_or("missing 'scenario' field")?
                .to_string();
            let mode_label = entry["mode"]
                .as_str()
                .ok_or("missing 'mode' field")?;
            let (engine, mode) = mode_label
                .split_once('-')
                .ok_or_else(|| format!(
                    "malformed mode label '{}': expected '<engine>-<durability>'",
                    mode_label
                ))?;
            let engine = engine.to_string();
            let mode = mode.to_string();
            let throughput = entry["throughput_ops_per_sec"]
                .as_f64()
                .ok_or("missing 'throughput_ops_per_sec' field")?;
            let p99_ns = entry["p99_ns"]
                .as_f64()
                .ok_or("missing 'p99_ns' field")?;
            samples
                .entry((scenario, engine, mode))
                .or_default()
                .push((throughput, p99_ns));
        }
    }

    // Compute per-cell COV and assemble the GateResult.
    let cells: Vec<CellResult> = samples
        .into_iter()
        .map(|((scenario, engine, mode), pairs)| {
            let throughput_samples: Vec<f64> = pairs.iter().map(|(t, _)| *t).collect();
            let p99_samples: Vec<f64> = pairs.iter().map(|(_, p)| *p).collect();
            let throughput = compute_cov(&throughput_samples);
            let p99_latency_ns = compute_cov(&p99_samples);
            let passes = throughput.cov <= cli.throughput_threshold
                && p99_latency_ns.cov <= cli.p99_threshold;
            CellResult { scenario, engine, mode, throughput, p99_latency_ns, passes }
        })
        .collect();

    let result = GateResult {
        provider: cli.provider.clone(),
        instance_type: cli.instance_type.clone(),
        run_count: cli.runs,
        throughput_threshold: cli.throughput_threshold,
        p99_threshold: cli.p99_threshold,
        cells,
    };

    let report = render_report(&result);
    fs::write(&cli.out, report)?;

    Ok(result.all_pass())
}
```

- [ ] **Step 3: Verify the binary builds**

```bash
cd bench && cargo build --bin chisel-bench-noise-gate 2>&1 | tail -10
```

Expected: clean build. (Note: if the binary auto-discovery names it `noise_gate` rather than `chisel-bench-noise-gate`, adjust by adding an explicit `[[bin]]` section in `bench/Cargo.toml`:

```toml
[[bin]]
name = "chisel-bench-noise-gate"
path = "src/bin/noise_gate.rs"
```

Verify by listing built binaries: `ls bench/target/debug/chisel-bench*`.)

- [ ] **Step 4: Verify the binary's --help works**

```bash
cd bench && cargo run --bin chisel-bench-noise-gate -- --help 2>&1 | tail -20
```

Expected: clap-rendered help text showing all the CLI options.

- [ ] **Step 5: Run the full bench test suite as a regression check**

```bash
cd bench && cargo test 2>&1 | tail -5
```

Expected: 107/107 still passing. (No new tests in Task 4.6 — this binary is exercised by Phase 4's Task 4.7 below.)

- [ ] **Step 6: Commit**

```bash
git add bench/src/bin/noise_gate.rs bench/Cargo.toml
git commit -m "bench: add chisel-bench-noise-gate CLI binary"
```

### Task 4.7: Run the noise gate on the provisioned VM

**Files:** None on the VM (operator action); the gate produces `noise-gate-report.md` in the working dir.

- [ ] **Step 1: Pull the latest commits onto the VM**

SSH to the VM:

```bash
ssh bench-op@chisel-bench-1
cd ~/work/chisel
git pull
```

- [ ] **Step 2: Build the noise-gate binary in release mode**

```bash
cd bench
cargo build --release --bin chisel-bench-noise-gate
```

- [ ] **Step 3: Run the gate**

For Hetzner CCX23:

```bash
cd ~/work/chisel/bench
./target/release/chisel-bench-noise-gate \
  --provider hetzner \
  --instance-type ccx23 \
  --runs 5 \
  --out /tmp/noise-gate-report.md
```

This will run `cargo bench --bench scenarios` 5 times back-to-back. Estimated runtime: ~50 minutes (10 minutes per run on Linux scenario tier).

Watch progress; if the gate fails, the report will tell you which cells exceeded threshold.

- [ ] **Step 4: Inspect the report**

```bash
cat /tmp/noise-gate-report.md
```

Expected: `**Verdict:** PASS (N / N cells under threshold)`. If FAIL, return to Task 1.1 with a different provider / instance type and start over.

- [ ] **Step 5: Commit the report into the repo as a forensic record**

From the operator workstation (not the VM):

```bash
# Copy from VM to local workstation
scp bench-op@chisel-bench-1:/tmp/noise-gate-report.md ./bench-results/noise-gate/

# Rename to the canonical path: <UTC>-<provider>-<instance>.md
UTC=$(date -u +%Y-%m-%dT%H-%M-%SZ)
mv bench-results/noise-gate/noise-gate-report.md \
   "bench-results/noise-gate/${UTC}-hetzner-ccx23.md"

git add "bench-results/noise-gate/${UTC}-hetzner-ccx23.md"
git commit -m "ops: record passing noise-gate report for hetzner ccx23"
git push origin main
```

The directory `bench-results/noise-gate/` will be created on first commit.

### Task 4.8: Phase 4 success check

- [ ] **Step 1: Confirm the noise-gate report committed**

```bash
git log -1 --format='%H %s' -- 'bench-results/noise-gate/*'
```

Expected: shows the noise-gate commit. The presence of a passing report in the repo is the success criterion for Phase 4.

---

## Phase 5: Per-PR workflow migration (bench.yml)

### Task 5.1: Add the --prepend-header CLI option to chisel-bench-diff

**Files:**
- Modify: `bench/src/bin/diff.rs`

- [ ] **Step 1: Read the current diff.rs to find the `Cli` struct and main flow**

```bash
cd bench && head -80 src/bin/diff.rs
```

Locate the `#[derive(Parser)] struct Cli` block (probably near the top).

- [ ] **Step 2: Add a failing test in diff.rs (or a new tests module)**

Append to `bench/src/bin/diff.rs` (or to a new `bench/src/diff/tests.rs` if the binary file is too cluttered for inline tests):

```rust
#[cfg(test)]
mod tests {
    // The header-prepending logic — minimal pure function so it's testable
    // without invoking the whole CLI.

    fn prepend_header(body: &str, header: Option<&str>) -> String {
        match header {
            Some(h) => format!("{h}\n\n{body}"),
            None => body.to_string(),
        }
    }

    #[test]
    fn prepend_header_with_none_returns_body_unchanged() {
        let body = "diff content here";
        assert_eq!(prepend_header(body, None), body);
    }

    #[test]
    fn prepend_header_with_some_prepends_with_blank_line() {
        let body = "diff content here";
        let header = "> ⚠️ warning text";
        let result = prepend_header(body, Some(header));
        assert_eq!(result, "> ⚠️ warning text\n\ndiff content here");
    }
}
```

- [ ] **Step 3: Run the test, verify it fails to compile**

```bash
cd bench && cargo test --bin chisel-bench-diff 2>&1 | tail -10
```

Expected: compile error because `prepend_header` is referenced in tests but not defined.

- [ ] **Step 4: Add the prepend_header function to the binary's main module**

In `bench/src/bin/diff.rs`, above the `fn main()` (or wherever helper functions live), add:

```rust
fn prepend_header(body: &str, header: Option<&str>) -> String {
    match header {
        Some(h) => format!("{h}\n\n{body}"),
        None => body.to_string(),
    }
}
```

Also add `--prepend-header` to the `Cli` struct:

```rust
/// If set, prepends this string to the rendered diff comment with a blank
/// line separator. Used by the bench-fallback workflow to flag that
/// numbers came from the shared GitHub-hosted runner rather than the
/// dedicated bench machine.
#[arg(long)]
prepend_header: Option<String>,
```

And update `main()` to use it. Locate where the diff body is written to the output file (currently something like `fs::write(&cli.out, body)?`) and change to:

```rust
let final_body = prepend_header(&body, cli.prepend_header.as_deref());
fs::write(&cli.out, final_body)?;
```

- [ ] **Step 5: Run the tests, verify they pass**

```bash
cd bench && cargo test --bin chisel-bench-diff 2>&1 | tail -10
```

Expected: 2/2 new tests pass; existing tests still pass.

- [ ] **Step 6: Verify the binary still builds and --help shows the new flag**

```bash
cd bench && cargo run --bin chisel-bench-diff -- --help 2>&1 | grep prepend-header
```

Expected: shows `--prepend-header <PREPEND_HEADER>` in the options.

- [ ] **Step 7: Commit**

```bash
git add bench/src/bin/diff.rs
git commit -m "bench: add --prepend-header to chisel-bench-diff for fallback warning"
```

### Task 5.2: Update bench.yml — add detect-runner job

**Files:**
- Modify: `.github/workflows/bench.yml`

- [ ] **Step 1: Read the current bench.yml structure**

```bash
cat .github/workflows/bench.yml | head -40
```

Identify the existing `jobs:` block and the single `bench:` job inside it.

- [ ] **Step 2: Add a `detect-runner` job above the existing `bench` job**

In `.github/workflows/bench.yml`, replace the `jobs:` section so it starts with:

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

  # bench-dedicated and bench-fallback added below in subsequent tasks
```

- [ ] **Step 3: Verify YAML parses**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/bench.yml'))" && echo "YAML valid"
```

Expected: `YAML valid`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/bench.yml
git commit -m "ci: add detect-runner job to bench.yml (dedicated-bench machine integration step 1/3)"
```

### Task 5.3: Update bench.yml — split the existing bench job into bench-dedicated and bench-fallback

**Files:**
- Modify: `.github/workflows/bench.yml`

This is the largest single change in the plan. The existing `bench` job's contents become the body of two new jobs that differ in `runs-on:`, in the `if:` gate, and in whether they prepend a warning header.

- [ ] **Step 1: Identify the body of the existing `bench:` job**

```bash
cat .github/workflows/bench.yml
```

Note all the steps inside `bench:` (checkout, setup-rust, build, run scenarios, summarize, diff, post comment, upload artifact). This body will be repeated almost verbatim in two jobs.

- [ ] **Step 2: Replace the existing `bench:` job with `bench-dedicated:` and `bench-fallback:`**

Below the `detect-runner:` job added in Task 5.2, replace the existing `bench:` block with the following two jobs. The body of each is the SAME steps the original `bench:` job had, with three differences:

  - `bench-dedicated`: runs on `[self-hosted, linux, dedicated-bench, bench-v1]`, gated by `if:` on dedicated-online AND non-fork; calls diff binary with NO header
  - `bench-fallback`: runs on `ubuntu-latest`, gated by `if:` on dedicated-offline OR fork; calls diff binary WITH a prepend-header

```yaml
  bench-dedicated:
    needs: detect-runner
    if: needs.detect-runner.outputs.use-dedicated == 'true' && github.event.pull_request.head.repo.full_name == github.repository
    runs-on: [self-hosted, linux, dedicated-bench, bench-v1]
    timeout-minutes: 60
    concurrency:
      group: dedicated-machine
      cancel-in-progress: false
    permissions:
      contents: read
      pull-requests: write
    steps:
      # All the original `bench:` steps go here, unchanged, EXCEPT the
      # diff-rendering step omits --prepend-header (no warning).
      #
      # IMPORTANT: copy-paste the existing bench: job body here and remove
      # the --prepend-header arg from the chisel-bench-diff invocation
      # (it shouldn't be set; if the existing job used it, drop it).
      - name: Checkout PR head
        uses: actions/checkout@v4
      # ... rest of body identical to the original bench: job ...

  bench-fallback:
    needs: detect-runner
    if: needs.detect-runner.outputs.use-dedicated == 'false' || github.event.pull_request.head.repo.full_name != github.repository
    runs-on: ubuntu-latest
    timeout-minutes: 60
    permissions:
      contents: read
      pull-requests: write
    steps:
      # SAME body as bench-dedicated, EXCEPT the chisel-bench-diff
      # invocation passes --prepend-header with the appropriate warning
      # text. Use a small env-var dispatch to choose the right header
      # text based on whether this is a fork PR or an offline-machine
      # fallback.
      - name: Checkout PR head
        uses: actions/checkout@v4
      # ... [most of the same steps as bench-dedicated] ...

      - name: Choose warning header text
        id: header
        run: |
          if [ "${{ github.event.pull_request.head.repo.full_name }}" != "${{ github.repository }}" ]; then
            echo "text=> ℹ️ **Fork PR — ran on shared GitHub-hosted runner** — fork PRs do not run on the dedicated bench machine for security reasons. Numbers have ±15% variance." >> $GITHUB_OUTPUT
          else
            echo "text=> ⚠️ **Ran on shared GitHub-hosted runner** — dedicated bench machine is offline. Numbers have ±15% variance; treat the diff signal as noisy. Re-trigger after the machine is back online for trustworthy values." >> $GITHUB_OUTPUT
          fi

      - name: Generate diff comment with warning header
        # This step replaces the original chisel-bench-diff invocation in bench:.
        # IMPORTANT: pass --prepend-header to inject the warning text.
        run: |
          ./bench/target/release/chisel-bench-diff \
            --baseline /tmp/baseline-results.json \
            --pr /tmp/pr-results.json \
            --prepend-header "${{ steps.header.outputs.text }}" \
            --out /tmp/diff-comment.md

      # ... [rest of the body, including PR comment post + artifact upload] ...
```

**Implementer note:** the `# ...` placeholders above stand for **the actual contents of the original `bench:` job** — checkout, Rust toolchain setup, build, run scenarios, run summarize on both the PR head and the baseline, post comment, upload artifact. Copy that body verbatim into both new jobs and modify only as noted. **The original `bench:` job is fully replaced** — there should be no remaining `bench:` job after this task.

**Required addition (per spec § "Disk management" — hard backstop):** insert a pre-job disk-check step at the top of BOTH `bench-dedicated` and `bench-fallback` job bodies (right after the Checkout step). This refuses to run if free disk is too low, preventing a bench run from filling the disk mid-execution and corrupting state. Use:

```yaml
      - name: Disk-space backstop (refuse to run if low)
        run: |
          AVAIL_GIB=$(df -BG / | tail -1 | awk '{print $4}' | tr -d 'G')
          echo "Available disk: ${AVAIL_GIB} GiB"
          if [ "$AVAIL_GIB" -lt 20 ]; then
            echo "::error title=Disk full::Free disk ${AVAIL_GIB} GiB is below the 20 GiB minimum. Run bench-disk-cleanup.yml or SSH in to investigate."
            exit 1
          fi
```

For `bench-fallback` running on `ubuntu-latest`, the check is essentially redundant (GitHub-hosted runners reset disk between jobs), but adding it uniformly keeps both jobs structurally identical and the backstop semantics consistent.

- [ ] **Step 3: Verify YAML parses and the workflow is valid**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/bench.yml'))" && echo "YAML valid"
```

If you have `actionlint` installed (recommended; install via `brew install actionlint`), run it as a stronger validator:

```bash
actionlint .github/workflows/bench.yml
```

Expected: no errors. If actionlint isn't available, the YAML check above is sufficient.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/bench.yml
git commit -m "ci: split bench job into bench-dedicated + bench-fallback (dedicated-bench machine integration step 2/3)"
```

### Task 5.4: Open a test PR to exercise the dedicated path

**Files:** None (operator action; the PR is throwaway)

- [ ] **Step 1: Create a throwaway branch with a no-op change**

```bash
git checkout -b throwaway/test-bench-dedicated
echo "" >> README.md
git add README.md
git commit -m "throwaway: trigger bench workflow"
git push origin throwaway/test-bench-dedicated
```

- [ ] **Step 2: Open a PR**

```bash
gh pr create --title "throwaway: test bench-dedicated path" \
  --body "Throwaway PR to verify bench.yml routes to the dedicated machine. Will be closed without merge." \
  --base main
```

Note the PR number for the next step.

- [ ] **Step 3: Watch the workflow run**

```bash
gh run watch
```

Expected: `detect-runner` runs first (~30s, on shared runner), then `bench-dedicated` runs (~10–25 min, on the self-hosted runner), then posts a sticky PR comment with NO warning header.

- [ ] **Step 4: Verify the PR comment**

Open the PR in a browser; confirm the bench-diff comment is posted and lacks any warning header. Comment should look like an existing bench-diff comment from prior PRs.

- [ ] **Step 5: Close the throwaway PR (don't merge)**

```bash
gh pr close <PR-NUMBER> --delete-branch
```

### Task 5.5: Open a second test PR with the runner forced offline

**Files:** None (operator action)

- [ ] **Step 1: Stop the runner service on the VM**

```bash
ssh bench-op@chisel-bench-1
sudo systemctl stop actions.runner.pgexperts-chisel.chisel-bench-1.service
```

The service name format is `actions.runner.<owner>-<repo>.<runner-name>.service`. Adjust if your registration used different names. Confirm via:

```bash
sudo systemctl list-units 'actions.runner.*' --type=service
```

- [ ] **Step 2: Confirm runner shows offline in GitHub UI**

Browser: `https://github.com/pgexperts/chisel/settings/actions/runners`. The `chisel-bench-1` runner should show "Offline".

- [ ] **Step 3: Create another throwaway PR**

```bash
git checkout main && git pull
git checkout -b throwaway/test-bench-fallback
echo "" >> README.md
git add README.md
git commit -m "throwaway: trigger bench workflow (fallback path)"
git push origin throwaway/test-bench-fallback
gh pr create --title "throwaway: test bench-fallback path" \
  --body "Throwaway PR to verify bench.yml falls back to ubuntu-latest when dedicated runner is offline." \
  --base main
```

- [ ] **Step 4: Watch the workflow**

```bash
gh run watch
```

Expected: `detect-runner` runs (~30s), then `bench-fallback` runs (~10 min on shared `ubuntu-latest`), then posts a sticky PR comment with the offline-warning header at the top.

- [ ] **Step 5: Verify the comment header**

Open the PR; confirm the bench-diff comment is prefixed with:

```
> ⚠️ **Ran on shared GitHub-hosted runner** — dedicated bench machine is offline. ...
```

- [ ] **Step 6: Restart the runner**

```bash
ssh bench-op@chisel-bench-1
sudo systemctl start actions.runner.pgexperts-chisel.chisel-bench-1.service
sudo systemctl status actions.runner.pgexperts-chisel.chisel-bench-1.service
```

Expected: `Active: active (running)`. Refresh the GitHub UI to confirm the runner is back to "Idle".

- [ ] **Step 7: Close the second throwaway PR**

```bash
gh pr close <PR-NUMBER> --delete-branch
```

### Task 5.6: Phase 5 success check

- [ ] **Step 1: Confirm both code paths produce the expected sticky comment**

This was verified by Tasks 5.4 and 5.5. The success criterion for Phase 5 is "both throwaway PRs produced the expected sticky comments — bench-dedicated without warning, bench-fallback with warning". No further commit needed.

---

## Phase 6: Disk-cleanup workflow

### Task 6.1: Create bench-disk-cleanup.yml

**Files:**
- Create: `.github/workflows/bench-disk-cleanup.yml`

- [ ] **Step 1: Write the workflow**

```yaml
name: Bench Disk Cleanup

# Nightly maintenance on the dedicated bench machine: prune old bench
# results, optionally cargo clean if disk is tight, drop apt caches.
# Emits a small disk-status.txt artifact for operator-visible trend.
#
# Spec: docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md
# § "Disk management"

on:
  schedule:
    - cron: '0 4 * * *' # nightly 04:00 UTC
  workflow_dispatch:

jobs:
  cleanup:
    runs-on: [self-hosted, linux, dedicated-bench, bench-v1]
    timeout-minutes: 30
    concurrency:
      group: dedicated-machine
      cancel-in-progress: false
    permissions:
      contents: read
    steps:
      - name: Show pre-cleanup disk state
        run: df -h /

      - name: Prune bench/results/<UTC>/ directories older than 14 days
        working-directory: ${{ github.workspace }}/bench
        run: |
          if [ -d results ]; then
            find results -mindepth 1 -maxdepth 1 -type d -mtime +14 -exec rm -rf {} +
          fi

      - name: cargo clean if free disk < 30 GiB
        working-directory: ${{ github.workspace }}/bench
        run: |
          AVAIL_GIB=$(df -BG / | tail -1 | awk '{print $4}' | tr -d 'G')
          echo "Available: ${AVAIL_GIB} GiB"
          if [ "$AVAIL_GIB" -lt 30 ]; then
            echo "Below 30 GiB — running cargo clean"
            cargo clean
          else
            echo "Above 30 GiB — keeping incremental cache"
          fi

      - name: apt-get clean
        run: sudo apt-get clean

      - name: Capture post-cleanup disk state
        run: |
          echo "Post-cleanup disk status (chisel-bench-1):" > /tmp/disk-status.txt
          echo "  Generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> /tmp/disk-status.txt
          df -h / >> /tmp/disk-status.txt

      - name: Upload disk-status artifact
        uses: actions/upload-artifact@v4
        with:
          name: disk-status-${{ github.run_id }}
          path: /tmp/disk-status.txt
          retention-days: 30
```

- [ ] **Step 2: Verify YAML parses**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/bench-disk-cleanup.yml'))" && echo "YAML valid"
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/bench-disk-cleanup.yml
git commit -m "ci: add bench-disk-cleanup.yml (nightly maintenance on dedicated runner)"
git push origin main
```

### Task 6.2: Trigger the workflow manually and verify

**Files:** None (operator action)

- [ ] **Step 1: Trigger via gh CLI**

```bash
gh workflow run bench-disk-cleanup.yml
```

- [ ] **Step 2: Watch the run**

```bash
gh run watch
```

Expected: workflow completes successfully (green check).

- [ ] **Step 3: Download and inspect the artifact**

```bash
RUN_ID=$(gh run list --workflow=bench-disk-cleanup.yml --limit 1 --json databaseId -q '.[0].databaseId')
mkdir -p /tmp/cleanup-check
gh run download "$RUN_ID" --dir /tmp/cleanup-check --name "disk-status-${RUN_ID}"
cat /tmp/cleanup-check/disk-status.txt
```

Expected: shows `df -h /` output for the VM with current disk usage.

### Task 6.3: Phase 6 success check

- [ ] **Step 1: Confirm the workflow completes and the artifact exists**

Already verified in Task 6.2. Phase 6 success criterion is "workflow completes; operator can `gh run download` the artifact". No additional commit.

---

## Phase 7: OS-update workflow

### Task 7.1: Create bench-os-update.yml

**Files:**
- Create: `.github/workflows/bench-os-update.yml`

- [ ] **Step 1: Write the workflow**

```yaml
name: Bench OS Update

# Monthly OS security upgrades on the dedicated bench machine. Reboots
# the VM only if (a) a kernel upgrade was applied AND (b) the runner
# queue is empty at the time. Otherwise defers reboot to the next
# month's run.
#
# Spec: docs/specs/2026-05-04-dedicated-bench-machine-foundation-design.md
# § "OS upgrades"

on:
  schedule:
    - cron: '0 5 1 * *' # monthly, 1st at 05:00 UTC
  workflow_dispatch:

jobs:
  update:
    runs-on: [self-hosted, linux, dedicated-bench, bench-v1]
    timeout-minutes: 30
    concurrency:
      group: dedicated-machine
      cancel-in-progress: false
    permissions:
      contents: read
    steps:
      - name: Show pre-upgrade state
        run: |
          uname -r
          apt list --upgradable 2>/dev/null | head -20

      - name: Apply security upgrades
        run: sudo unattended-upgrade -d

      - name: Determine if reboot is needed
        id: reboot-check
        run: |
          if [ -f /var/run/reboot-required ]; then
            echo "needed=true" >> $GITHUB_OUTPUT
            cat /var/run/reboot-required.pkgs 2>/dev/null || true
          else
            echo "needed=false" >> $GITHUB_OUTPUT
          fi

      - name: Check queue depth before reboot
        if: steps.reboot-check.outputs.needed == 'true'
        id: queue-check
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          # Count queued + in-progress jobs targeting this runner. Excludes
          # the current job (we ARE in-progress).
          QUEUED=$(gh api repos/${{ github.repository }}/actions/runs \
            --jq '[.workflow_runs[]
                   | select(.status == "queued" or .status == "in_progress")
                   | select(.id != ${{ github.run_id }})
                  ] | length')
          echo "queued=${QUEUED}" >> $GITHUB_OUTPUT
          echo "Queue depth: ${QUEUED}"

      - name: Reboot if queue is empty
        if: steps.reboot-check.outputs.needed == 'true' && steps.queue-check.outputs.queued == '0'
        run: |
          echo "Reboot needed and queue is empty — rebooting in 1 minute"
          sudo shutdown -r +1 "Bench OS update reboot"

      - name: Skip reboot (queue not empty or no kernel patch)
        if: steps.reboot-check.outputs.needed == 'false' || steps.queue-check.outputs.queued != '0'
        run: |
          echo "Skipping reboot."
          echo "  Reboot needed: ${{ steps.reboot-check.outputs.needed }}"
          echo "  Queue depth: ${{ steps.queue-check.outputs.queued }}"
          echo "Next month's run will retry if reboot is still needed."
```

- [ ] **Step 2: Verify YAML parses**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/bench-os-update.yml'))" && echo "YAML valid"
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/bench-os-update.yml
git commit -m "ci: add bench-os-update.yml (monthly security upgrades on dedicated runner)"
git push origin main
```

### Task 7.2: Trigger the workflow and verify

**Files:** None (operator action)

- [ ] **Step 1: Trigger via gh CLI**

```bash
gh workflow run bench-os-update.yml
```

- [ ] **Step 2: Watch the run**

```bash
gh run watch
```

Expected: workflow completes successfully.

- [ ] **Step 3: Verify the upgrade ran**

SSH to the VM and confirm:

```bash
ssh bench-op@chisel-bench-1
sudo cat /var/log/unattended-upgrades/unattended-upgrades.log | tail -30
```

Expected: shows the most recent unattended-upgrade run timestamp matching the workflow run.

- [ ] **Step 4: If reboot was triggered, verify the runner came back**

If the workflow's "Reboot if queue is empty" step ran, the VM rebooted. Wait 2–3 minutes, then:

```bash
ssh bench-op@chisel-bench-1 uptime
```

Expected: low uptime (recent reboot); SSH works; runner shows as "Idle" in GitHub UI again.

If no reboot was needed (no kernel patch this month) or the queue wasn't empty, no reboot happened — that's also success.

### Task 7.3: Phase 7 success check

- [ ] **Step 1: Confirm the workflow completes and the upgrade ran**

Already verified in Task 7.2. Phase 7 success criterion is "workflow completes; `apt list --upgradable` on the machine shows nothing or only non-security packages". Verify:

```bash
ssh bench-op@chisel-bench-1 'apt list --upgradable 2>/dev/null'
```

Expected: empty output, or only non-security packages.

---

## Phase 8: Operator runbook

### Task 8.1: Write the runbook

**Files:**
- Create: `docs/operations/dedicated-bench-runbook.md`

- [ ] **Step 1: Verify the docs/operations/ directory exists or create it**

```bash
mkdir -p docs/operations
```

- [ ] **Step 2: Write the runbook**

Create `docs/operations/dedicated-bench-runbook.md` with the following content. (The outer fence below uses four backticks so the inner three-backtick fences nest correctly; when you write the file, use plain three-backtick fences inside.)

````markdown
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
sudo systemctl stop actions.runner.pgexperts-chisel.chisel-bench-1.service
```

While stopped: per-PR work falls through to `ubuntu-latest` automatically (with the offline-warning header on the PR comment). Canonical and soak runs queue in the GitHub Actions UI until the runner returns. After maintenance:

```bash
sudo systemctl start actions.runner.pgexperts-chisel.chisel-bench-1.service
sudo systemctl status actions.runner.pgexperts-chisel.chisel-bench-1.service
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
# Diff: chisel-bench-diff writes the rendered markdown to stdout — redirect.
cargo run --release --bin chisel-bench-diff -- \
  --baseline /tmp/main-out/results.json \
  --pr /tmp/pr-out/results.json \
  > /tmp/diff.md
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
  --url https://github.com/pgexperts/chisel \
  --token <REGISTER-TOKEN> \
  --name chisel-bench-1 \
  --labels dedicated-bench,bench-v1 \
  --disableupdate \
  --unattended

sudo ./svc.sh install bench-op
sudo ./svc.sh start
```

Get the REMOVE/REGISTER tokens from `https://github.com/pgexperts/chisel/settings/actions/runners`.

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
sudo systemctl restart actions.runner.pgexperts-chisel.chisel-bench-1.service
```

### VM is unresponsive (SSH hangs)

Hard reboot via cloud-provider console (Hetzner: Console → Server → Power → Reset; AWS: EC2 Console → Instance State → Reboot). After ~2 minutes, SSH should work again. The runner systemd service is `Restart=always`, so it'll be back online on its own.

If the VM doesn't come back from reboot, attach the cloud-provider rescue console to investigate filesystem corruption.
````

- [ ] **Step 3: Verify the file exists and renders sensibly**

```bash
ls -la docs/operations/dedicated-bench-runbook.md
head -30 docs/operations/dedicated-bench-runbook.md
```

- [ ] **Step 4: Commit**

```bash
git add docs/operations/dedicated-bench-runbook.md
git commit -m "docs: add operator runbook for dedicated bench machine"
git push origin main
```

### Task 8.2: Phase 8 success check

- [ ] **Step 1: Operator (you) reviews the runbook**

Read through `docs/operations/dedicated-bench-runbook.md` end-to-end. Confirm:
- All procedures have concrete commands (no placeholders)
- The "Machine inventory" section has been filled in with your actual provider/IP after deployment
- You'd be able to recover from each "Common procedures" scenario without further guidance

If anything's vague, edit and re-commit.

---

## Phase 9: Foundation v1 ships when

The spec's success criteria, restated for operator visibility:

- [ ] All 8 phases complete (Tasks 1.x through 8.x checked off)
- [ ] Noise-gate report committed to `bench-results/noise-gate/<UTC>-<provider>-<instance>.md`
- [ ] At least one of Specs 2/3/4 implemented end-to-end on top of the foundation (most likely Spec 3, since Phase 5 already lays the groundwork)
- [ ] Operator runbook reviewed (Task 8.2)

When all four are true, declare Spec 1 v1 done and close the foundation milestone in CLAUDE.md (or the project's equivalent status doc).

---

## Cross-cutting verification commands

Reference: any time you want a quick sanity check, run these from the chisel repo root:

```bash
# Repo state
git status
git log --oneline -10

# Bench tests
cd bench && cargo test 2>&1 | tail -3 && cd ..

# Workflow YAML validity
for f in .github/workflows/bench*.yml; do
  python3 -c "import yaml; yaml.safe_load(open('$f'))" && echo "✓ $f"
done

# Runner online?
gh api repos/pgexperts/chisel/actions/runners --jq '.runners[] | {name, status, labels: [.labels[].name]}'
```
