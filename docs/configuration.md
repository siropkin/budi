# budi Configuration Reference

Config lives at `~/.local/share/budi/repos/<repo-id>/config.toml`. Every field has a default — an empty file is valid. Unknown fields are silently ignored.

Find the path for the current repo:

```bash
budi doctor  # prints the data directory path
```

---

## Daemon

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `daemon_host` | string | `"127.0.0.1"` | Host the daemon binds to. Change to `"0.0.0.0"` only in trusted environments. |
| `daemon_port` | integer | `7878` | Port the daemon listens on. Change if 7878 conflicts with another service. |

---

## Debug / Telemetry

All debug fields are **off by default** and have no runtime cost when disabled.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `debug_io` | bool | `false` | Log every hook event to `logs/hook-io.jsonl`. Useful for inspecting what budi processes. |

### Reading the debug log

```bash
tail -n 50 ~/.local/share/budi/repos/<repo-id>/logs/hook-io.jsonl | jq .
```

### Example config for debugging

```toml
debug_io = true
```
