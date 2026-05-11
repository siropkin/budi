# Architecture Decision Records

This directory holds the durable architecture decisions for Budi. Each ADR captures the context, decision, and consequences of one choice. Status values:

- **Accepted** — the ADR is in force. May carry banner amendments that narrow or extend specific sections without invalidating the record.
- **Superseded by ADR-XXXX** — replaced in full by a later ADR. Kept for historical context; do not act on its decisions.
- **Deprecated** — no longer in force and not replaced by a successor ADR. Kept for historical context.

When an ADR is amended (banner at top) the rest of the record still stands except for the sections the amendment names. When an ADR is superseded, read the successor for the current contract.

## Index

| ADR | Title | Status | Summary |
|-----|-------|--------|---------|
| [0081](./0081-product-contract-and-deprecation-policy.md) | 8.0 Product Contract and Deprecation Policy | Accepted (amended by [0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)) | Locks the 8.0 product contract, surface taxonomy, and deprecation policy for the proxy-first / cloud-enabled pivot. |
| [0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md) | Proxy Compatibility Matrix and Local Gateway Contract | Superseded by [0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) | Original proxy-first agent compatibility matrix, gateway contract, and `X-Budi-*` attribution header protocol. Retired in 8.2 R2.1. |
| [0083](./0083-cloud-ingest-identity-and-privacy-contract.md) | Cloud Ingest, Identity, and Privacy Contract | Accepted (amended by [0091](./0091-model-pricing-manifest-source-of-truth.md) and [0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md)) | Defines what data leaves the machine, how identity and dedup work, and the outbound-network surface for the cloud layer. |
| [0086](./0086-extraction-boundaries.md) | Extraction Boundaries for budi-cursor and budi-cloud | Accepted | Locks repo boundaries and API contracts between `budi-core`, `budi-cursor`, and `budi-cloud` ahead of monorepo extraction. |
| [0087](./0087-cloud-infrastructure-and-deployment.md) | Cloud Infrastructure, Deployment, and Domain Strategy | Accepted | Pins Supabase dev/prod separation, domain strategy, dashboard auth, daemon lifecycle, and the cloud-side deployment topology. |
| [0088](./0088-8x-local-developer-first-product-contract.md) | 8.x Local-Developer-First Product Contract | Accepted (amended by [0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) and [#648](https://github.com/siropkin/budi/issues/648)) | Sets the 8.x persona priority (local-developer-first), round order, statusline contract, classification intent, and host- vs. provider-scoped surface rule. |
| [0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) | Reverse Proxy-First Architecture — JSONL Tailing as Sole Live Path | Accepted | Reverses the proxy-first contract: JSONL tailing of agent transcripts becomes the sole live ingestion path; the proxy is removed. Supersedes [0082](./0082-proxy-compatibility-matrix-and-gateway-contract.md). |
| [0090](./0090-cursor-usage-api-contract.md) | Cursor Usage API Contract | Accepted | Pins the undocumented `cursor.com/api/dashboard/*` endpoints, auth, headers, and response shapes that the Cursor provider depends on. |
| [0091](./0091-model-pricing-manifest-source-of-truth.md) | Model Pricing via Embedded Baseline + LiteLLM Runtime Refresh | Accepted (amended by [0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md)) | Replaces hand-rolled per-provider pricing functions with an embedded baseline manifest plus optional LiteLLM runtime refresh; extends the outbound-network surface. |
| [0092](./0092-copilot-chat-data-contract.md) | Copilot Chat Data Contract (Local Tail + GitHub Billing API) | Accepted (companion: [0093](./0093-copilot-chat-jetbrains-storage-shape.md) for the JetBrains host) | Pins the Copilot Chat local JSON/JSONL tail surface and the GitHub Billing API truth-up contract used by the `copilot_chat` provider. |
| [0093](./0093-copilot-chat-jetbrains-storage-shape.md) | Copilot Chat — JetBrains Host Storage Shape | Accepted | Companion to [0092](./0092-copilot-chat-data-contract.md). Pins the JetBrains-side Xodus + Nitrite binary dual-store layout under `~/.config/github-copilot/` and the consequence that local-tail attribution is metadata-only — token reconciliation falls back to the GitHub Billing API. |
| [0094](./0094-custom-team-pricing-and-effective-cost-recalculation.md) | Custom Team Pricing and Effective-Cost Recalculation | Proposed | Splits cost columns into immutable `_ingested` and recomputable `_effective`. Adds cloud-side CSV-driven price-list authoring + recalculation engine, and a `GET /v1/pricing/active` endpoint the local daemon polls to mirror team rates so local and cloud display the same dollar amount. Amends [0091](./0091-model-pricing-manifest-source-of-truth.md) §5 and [0083](./0083-cloud-ingest-identity-and-privacy-contract.md) §Neutral. |

## Numbering

ADR numbers are assigned chronologically and never reused. Gaps in the sequence (e.g. missing 0084 / 0085) reflect numbers reserved during drafting that did not land — they should not be reissued.
