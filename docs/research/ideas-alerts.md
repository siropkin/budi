# Alerts / Signals System Design

- **Date**: 2026-03-24
- **Status**: Proposed (partially planned in R5 #106, #107)
- **Related issues**: #106 (budget engine warn-only), #107 (hard budget blocking)

## Concept

Alerts are a separate entity from tags. Tags are for attribution/grouping. Alerts are for thresholds and notifications.

BurnRate has per-project budget caps. Budi should have a more flexible system since it has tags — alerts can scope by any tag dimension.

## Config

`~/.config/budi/alerts.toml`:

```toml
[[alerts]]
name = "daily-spend"
description = "Daily spend limit"
metric = "cost_cents"          # cost_cents | tokens | sessions
period = "today"               # today | week | month | 7d | 30d
threshold = 5000               # in cents = $50
scope = {}                     # optional: { provider = "claude_code", repo = "budi" }

[[alerts]]
name = "opus-heavy"
description = "Too much Opus usage"
metric = "cost_cents"
period = "today"
threshold = 2000
scope = { tag = "model:*opus*" }
```

## Entities

- **AlertConfig**: name, description, metric, period, threshold, scope (optional filters)
- **AlertState**: current_value, triggered (bool), triggered_at, acknowledged (bool)
- No DB table needed initially — evaluate config against rollup tables periodically
- In 8.0, data arrives via proxy (real-time) and rollup triggers update aggregates — alerts evaluate against these

## Surfaces

- **Statusline**: Warning icon when any alert is triggered (e.g., `! $52.10/day`)
- **Dashboard**: Alert banner at top when triggered, with dismiss/acknowledge
- **CLI**: `budi alerts` — show current alert status

## Future Extensions

- Webhook/shell command on trigger
- Slack notifications (cloud tier)
- Per-team alerts (cloud tier)
- Rate-of-change alerts ("spending 3x faster than yesterday")
- Cloud-pushed budget policies (post-8.0, see ADR-0083 trade-offs)

## 8.0 Alignment Notes

- R5 issues #106 (warn-only) and #107 (hard blocking) implement the budget engine at the proxy layer
- Per ADR-0083, budget evaluation and enforcement are **local-first** — the cloud provides visibility but not enforcement
- Proxy-based alerts can react in real-time (per-request cost threshold), not just periodic rollup evaluation
- Hard blocking (#107) means the proxy can reject requests when budget is exceeded — more powerful than alert-only
