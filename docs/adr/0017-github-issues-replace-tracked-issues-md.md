---
id: 0017
title: Track issues in GitHub, not a tracked ISSUES.md
date: 2026-08-03
status: Accepted
summary: The 1868-line ISSUES.md decision log was retired; open entries were migrated to GitHub issues and the file deleted.
---

# 0017. Track issues in GitHub, not a tracked ISSUES.md

## Context

From the project's start, issues lived in a single git-tracked `ISSUES.md`:
119 numbered entries (`I1`–`I160`, with gaps) across 1868 lines, each carrying
a location, a triaged problem statement, and a direction of fix. It doubled as
the decision log — closed entries recorded not just that something was fixed
but why a particular fix was chosen over the alternatives.

Three forces made this untenable:

- **It outgrew a context window.** At 1868 lines the file could not be read
  whole by an agent session that also needed to hold the code. Every review
  pass had to grep it, and the 2026-07-29 review deliberately withheld it from
  its reviewers to keep the pass clean-slate — an admission that the log had
  become too large to be an input.
- **It duplicated a tracker the project already used.** The repository is
  public and its work already flows through GitHub issues and pull requests.
  The 2026-07-29 review's findings were filed as issues #102–#126 while their
  predecessors sat in `ISSUES.md`, so the same finding could exist in two
  places under two identifiers (the review's own text had to annotate findings
  "KNOWN as I149" to reconcile them).
- **Status drifted silently.** Nothing linked an entry to the commit that
  fixed it. At the time of this decision `I149` and `I150` were both still
  marked `🔶 OPEN` although `I150` had been fixed and merged in PR #128 and
  `I149` was fixed in the branch performing this migration.

Doing nothing meant maintaining a second issue tracker by hand, in a format
that could not be queried, linked from a commit, or closed by a merge.

## Decision

We will track issues in GitHub and delete `ISSUES.md`.

The 14 open and 3 deferred entries were triaged before deletion: 6 were
already represented in existing GitHub issues (`I147`→#116, `I149`/`I150`→#102,
`I151`→#107, `I152`/`I154`→#106) and the remaining 11 were filed as issues
#138–#148, each carrying its original entry verbatim plus a provenance header
naming its `I<number>` and source review. The 83 entries marked fixed were not
migrated; they describe completed work and remain readable in git history.

The ~167 `I<number>` markers in source comments were deliberately **not**
rewritten. `ARCHITECTURE.md` gained a note explaining what they refer to and
how to retrieve the retired file.

## Alternatives considered

- **Keep `ISSUES.md` and sync it with GitHub** — rejected. Two systems of
  record with a manual sync step is exactly the drift that produced the stale
  `I149`/`I150` statuses. The sync would have no enforcement.

- **Split `ISSUES.md` into per-issue files under `docs/issues/`**, mirroring
  what was done for the ADR log — rejected. It solves the context-window
  problem but not the duplication: the project would still be running a
  second tracker alongside GitHub, with no way to close an entry from a PR.
  The ADR split is different in kind, because decisions genuinely belong in
  the repository at the commit that made them; open work does not.

- **Migrate the 83 fixed entries into ADRs** — rejected for this change, but
  worth revisiting. Some closed entries do contain real decision rationale
  (the `I119` argument for a controlled panic over a typed fatal error, for
  instance). Converting them wholesale would be a large paraphrase-risk
  exercise; doing it selectively, when a future change touches the decision,
  is cheaper and more accurate.

- **Rewrite the ~167 `I<number>` comment markers** to point at the new issue
  numbers — rejected. Most refer to entries that were closed years of commits
  ago and have no GitHub equivalent, so the rewrite would be mostly deletion
  of provenance, across 167 sites, for no navigational gain.

## Consequences

- Issue status is now derived from GitHub rather than asserted in a file: a
  merged PR can close its issue, and an issue cannot be stale-but-marked-open
  without someone noticing.
- Labels (`severity:*`, `type:*`) replace the `P1`/`P2`/`P3` priority prefix.
  The migrated issues were labelled by nature, not by mechanical translation
  of their old priority, so old and new priorities are not comparable.
- The decision log is now split by kind: `docs/adr/` holds decisions, GitHub
  holds open work. A reader looking for "why is it like this" goes to the ADRs;
  a reader looking for "what is broken" goes to the issue tracker.
- The 83 fixed entries are no longer discoverable by grep in a checkout. They
  remain in git history (`git show 0ffe3bc:ISSUES.md`), but retrieving them now
  requires knowing they existed — which is what the `ARCHITECTURE.md` note is
  for. This is the real cost of this decision.
- Offline work loses the issue list. Anyone working without network access can
  no longer read the open issues from the checkout.
- The decision log itself now contains dead citations. Records 0006, 0007,
  0012, 0013 and 0015 were written while `ISSUES.md` was live and cite it by
  `I<number>`; those bodies are Accepted and are superseded rather than edited,
  so the citations stay. This is the same trade made for the ~167 code comments
  above, and it lands in a worse place — README.md and ARCHITECTURE.md now route
  readers to `docs/adr/` as *the* decision log, so a reader following that
  pointer can hit a citation with no way to resolve it. Record 0000 carries the
  breadcrumb (`git show 0ffe3bc:ISSUES.md`) for the whole log rather than
  repeating it in each affected record.
