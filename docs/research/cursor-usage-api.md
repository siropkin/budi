# Cursor Usage API Research

- **Date**: 2026-03-25
- **Status**: Verified (undocumented API, may change)
- **Used by**: `crates/budi-core/src/providers/cursor.rs` (historical import via `budi import`)

## Endpoints

### Filtered Usage Events (per-request, JSON)

```
POST https://cursor.com/api/dashboard/get-filtered-usage-events
```

Response:
```json
{
  "totalUsageEventsCount": 4980,
  "usageEventsDisplay": [
    {
      "timestamp": "1774455909363",
      "model": "composer-2-fast",
      "kind": "USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS",
      "tokenUsage": {
        "inputTokens": 2958,
        "outputTokens": 1663,
        "cacheReadTokens": 48214,
        "totalCents": 1.68
      },
      "chargedCents": 0,
      "isChargeable": false,
      "isTokenBasedCall": false,
      "owningUser": "273223875",
      "owningTeam": "9890257"
    }
  ]
}
```

### CSV Export (all events in billing period)

```
GET https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens
```

Columns: Date, Kind, Model, Max Mode, Input (w/ Cache Write), Input (w/o Cache Write), Cache Read, Output Tokens, Total Tokens, Cost

### Basic Usage (aggregate)

```
POST https://cursor.com/api/dashboard/get-current-period-usage
```

Returns: `{ billingCycleStart, billingCycleEnd, displayThreshold }`

## Authentication

Token in the same `state.vscdb` that budi reads for Cursor provider:

- **Table**: `ItemTable` (not cursorDiskKV)
- **Key**: `cursorAuth/accessToken`
- **Value**: JWT
- **User ID**: decode JWT payload -> `sub` field -> split on `|` -> second part

Cookie format: `WorkosCursorSessionToken={userId}%3A%3A{JWT}`

Required headers (CSRF protection):
- `Origin: https://cursor.com`
- `Referer: https://cursor.com/dashboard`
- Base URL: `https://cursor.com` (no www — www returns 308 redirect)

## Caveats

- Undocumented API — may change without notice
- Cloudflare challenge may block curl/ureq (needs browser JS engine)
- JWT expires but Cursor auto-refreshes in state.vscdb
- No conversation_id in API events — correlate by timestamp to sessions
- Returns current billing period only (not historical)
- 4,980 events verified in single billing period (Mar 2026)
- `kind` values: `USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS`, `FREE_CREDIT`, `USAGE_BASED`
