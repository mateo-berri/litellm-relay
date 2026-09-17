# Claude Desktop onboarding

Relay wires the Claude Desktop app (third-party gateway mode) onto your LiteLLM AI Gateway so it boots straight into gateway mode with no Claude.ai account and no key handling by the developer.

`relay onboard-claude-desktop` writes the OS-native managed configuration Claude Desktop reads on launch — `/etc/claude-desktop/managed-settings.json` on Linux — pointing inference at the Gateway. The Gateway must implement the Anthropic Messages API (`POST /v1/messages`), which LiteLLM does.

## Single sign-on (recommended)

Each developer signs in with their corporate account; the resulting OIDC token is sent to the Gateway as the bearer credential, so no provider or gateway key lands on the device.

```bash
sudo relay onboard-claude-desktop \
  --gateway-url https://gateway.yourco.com \
  --oidc-client-id "$CLIENT_ID" \
  --oidc-issuer https://login.yourco.com/v2.0
```

## Relay-issued credential

When Relay is onboarded against an IdP (`relay onboard --authorize-url ...`), no credential flag is needed. Relay exchanges the developer's IdP sign-in for a Gateway credential at the Gateway's `/token` endpoint and writes it as the key Claude Desktop sends, so the Gateway attributes the app's traffic to the developer and their team without an admin-issued key.

```bash
sudo relay onboard-claude-desktop --gateway-url https://gateway.yourco.com
```

Run from a terminal, the command opens the browser sign-in when no identity token is cached. Without a terminal (an MDM push, the auto-configure daemon) it never opens a browser: it reuses the identity the developer already signed in with, so sign in once (`relay claude-token`, `relay codex-token`, or the command above) before its first run. The daemon renews the credential with its refresh token and rewrites the managed file on every run (hourly by default), so the key in the file always has close to its full lifetime left. Claude Desktop reads the file on launch, so an app left running for longer than the credential's lifetime (24 hours by default) needs a restart to pick up the renewed one. When the exchange fails and a Gateway key is already saved in Relay's config, Relay keeps that key and reports the failure on stderr.

## Static key (proof of concept)

Distribute a shared Gateway key instead of a per-developer sign-in. An explicit `--api-key` wins over the Relay-issued credential.

```bash
sudo relay onboard-claude-desktop \
  --gateway-url https://gateway.yourco.com \
  --api-key sk-your-gateway-key
```

The managed file must be root-owned (Claude Desktop ignores a user-writable one), so run the command with `sudo`. Restart Claude Desktop to pick up the configuration. Because the managed settings are OS-native, this is the surface you push through your MDM — see [mdm.md](mdm.md).

## Usage

The developer only launches Claude Desktop. It opens on the gateway welcome screen ("Your organization has set up Claude to run through a custom inference gateway. No Claude.ai account needed.") and answers through the Gateway.

![Claude Desktop gateway welcome screen](img/claude-desktop-welcome.png)

![Claude Desktop answering through the Gateway](img/claude-desktop-answer.png)

## Demo

Claude Desktop and Codex, both onboarded by Relay and answering through one LiteLLM Gateway with zero developer setup:

[▶ Watch the demo (mp4)](video/claude-desktop-codex-vscode-gateway-demo.mp4)
