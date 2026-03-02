# Anthropic Plugin Submission Packet: budi-hooks

This document is a ready-to-submit packet for the Anthropic Claude Code plugin
directory form.

## Submission links

- https://platform.claude.com/plugins/submit
- https://claude.ai/settings/plugins/submit

## Plugin identity

- Plugin name: `budi-hooks`
- Marketplace name: `budi-plugins`
- Repository: `https://github.com/siropkin/budi`
- Marketplace manifest: `.claude-plugin/marketplace.json`
- Plugin manifest: `plugins/budi-hooks/.claude-plugin/plugin.json`
- Version: `1.0.7`

## Short description

Claude Code hooks that run `budi` for prompt-time context injection and
post-edit indexing updates in large repositories.

## What it installs

- `UserPromptSubmit` hook
  - command: `budi hook user-prompt-submit`
- `PostToolUse` hook (matcher: `Write|Edit`)
  - command: `budi hook post-tool-use`
  - async: `true`
  - timeout: `30`

## User value

- Better grounding in large codebases without manual context hunting
- Faster follow-up responses by pre-injecting deterministic project context
- Automatic index updates after edits

## Security and trust notes

- Hooks execute only local shell commands invoking the `budi` binary.
- No plugin-packaged remote MCP servers.
- No plugin-packaged API keys or credentials.
- Source is auditable in a public repository.

## Install instructions for reviewers

```shell
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

## Validation evidence

- Marketplace + plugin schema checks in CI:
  - `.github/workflows/validate-plugins.yml`
- `claude plugin validate .`
- `claude plugin validate plugins/budi-hooks`
- JSON syntax checks for plugin and marketplace manifests
- Hook matcher/command policy checks
- Security checks for hardcoded secrets/private keys

## Assets to provide in form

- Repo URL: `https://github.com/siropkin/budi`
- Plugin homepage/docs: `https://github.com/siropkin/budi/tree/main/plugins/budi-hooks`
- Marketplace install command and plugin install command
- Hook behavior summary (events, matchers, commands)
- Safety notes above

## Submission status

- Prepared: yes
- Submitted in Anthropic form: pending manual account action by repository owner
