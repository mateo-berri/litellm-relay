# ai_tools

Onboarding for AI coding tools onto the LiteLLM AI Gateway. Each tool is wired so it authenticates with the developer's corporate identity and routes through the Gateway, with no provider API key on the device.

## Layout

Shared identity concerns live at the top level and are reused by every tool. `idp.rs` runs the loopback browser sign-in against the corporate IdP and returns a JWT. `token.rs` caches that JWT under `~/.litellm-relay/` and refreshes it when it is missing or near expiry. `gateway_credential.rs` registers Relay with the Gateway's authorization server (`/.well-known/litellm-cli-auth`), exchanges the JWT for a Gateway credential (RFC 8693 token exchange at `/token`), caches it per Gateway and team under `~/.litellm-relay/gateway-credentials.json`, and keeps it fresh with its refresh token. Both caches are written through `system::write_private`, which keeps them owner-only and owned by the developer even when the root daemon writes them. None of these modules knows anything about a specific tool.

`detect.rs` decides which tools are installed on the device (pure `PATH`/filesystem inspection via `DetectContext`), and `autoconfigure.rs` drives detection then calls each detected tool's `onboard`, continuing past any single tool's failure. This is what makes Relay opt-out: installing it wires every recognized tool to the Gateway, no per-tool command.

Each tool gets its own folder holding only that tool's settings writer, for example `claude_cli/` for Claude Code. Codex and others are added as sibling folders without touching the shared modules or Relay's proxy code.

```
ai_tools/
  mod.rs          shared re-exports and module wiring
  detect.rs       which AI tools are installed (shared)
  autoconfigure.rs detect + onboard every installed tool (shared)
  idp.rs          corporate IdP browser sign-in (shared)
  token.rs        identity token cache and refresh (shared)
  gateway_credential.rs  IdP token to Gateway credential exchange and cache (shared)
  claude_cli/     Claude Code settings writer
    mod.rs
```

## Adding a tool

Create a folder named after the tool. Print the bearer through `gateway_credential::print_bearer` (which resolves the identity token through `token::ensure_token` and exchanges it) rather than talking to `token` or `idp` directly, so sign-in, exchange, caching, and refresh stay in one place. Write only the tool's own config (its equivalent of `~/.claude/settings.json`), pointing it at the Gateway with the team header and an `apiKeyHelper`-style hook that prints the credential to stdout and nothing else. Wire the new `onboard` and token commands into `src/app.rs`. Then teach `detect.rs` how to recognize the tool (add an `AiTool` variant and its detection evidence) and dispatch it in `autoconfigure.rs` so it is auto-configured on install. Add meaningful tests next to the writer.
