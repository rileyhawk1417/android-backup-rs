# Android Backup Utility — Build Plan

**Start here:** [docs/plan/index.md](plan/index.md) — the executable build plan.

## Document map

| Document | Purpose |
|----------|---------|
| [`plan/index.md`](plan/index.md) | **Agent entry point** — wave order, rules, progress tracker |
| `plan/wave-0` … `wave-8` | Per-wave task docs with instructions + acceptance gates |
| [`overview_plan.md`](overview_plan.md) | Authoritative product/architecture plan (cited as §N) |
| [`feature_tasks.md`](feature_tasks.md) | Feature-by-feature view (F-numbers) |
| [`task_breakdown.md`](task_breakdown.md) | Original task IDs (E-numbers derive from these) |

## Core idea

The backup engine (Wave 5) is blocked by five prerequisites (ADB runner, manifest
DB, device profiles, validated destinations, remote enumeration). The wave order
resolves those blocks in topological sequence, so the engine — the riskiest,
most central component — is never built on unfinished ground. Managed ADB
installation blocks nothing, so it is scheduled late (Wave 7).