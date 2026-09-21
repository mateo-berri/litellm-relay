# Claude Desktop onboarding

Relay wires the Claude Desktop app (third-party gateway mode) onto your LiteLLM AI Gateway so it boots straight into gateway mode with no Claude.ai account and no key handling by the developer.

`relay onboard-claude-desktop` writes the OS-native managed configuration Claude Desktop reads on launch, pointing inference at the Gateway: on macOS the `com.anthropic.claudefordesktop` managed preferences domain (`/Library/Managed Preferences/com.anthropic.claudefordesktop.plist`), on Linux `/etc/claude-desktop/managed-settings.json`. The Gateway must implement the Anthropic Messages API (`POST /v1/messages`), which LiteLLM does.

## Single sign-on (recommended)

Each developer signs in with their corporate account; the resulting OIDC token is sent to the Gateway as the bearer credential, so no provider or gateway key lands on the device.

```bash
sudo relay onboard-claude-desktop \
  --gateway-url https://gateway.yourco.com \
  --oidc-client-id "$CLIENT_ID" \
  --oidc-issuer https://login.yourco.com/v2.0
```

## Static key (proof of concept)

Distribute a shared Gateway key instead of SSO.

```bash
sudo relay onboard-claude-desktop \
  --gateway-url https://gateway.yourco.com \
  --api-key sk-your-gateway-key
```

The managed location is root-owned and Claude Desktop ignores a user-writable copy, so run the command with `sudo`. Relay reads the file back after writing and fails instead of reporting success when Claude Desktop could not pick it up, including when a per-user managed plist (`/Library/Managed Preferences/<account>/com.anthropic.claudefordesktop.plist`, which the app reads over the host-level one) already sets the inference keys. On macOS it also deletes the `/etc/claude-desktop/managed-settings.json` that earlier Relay versions wrote there, a file the macOS app never reads. Restart Claude Desktop to pick up the managed configuration. On an MDM-enrolled Mac a managed-preferences refresh (login, a profile push) regenerates `/Library/Managed Preferences` from the installed profiles and drops the plist; the root LaunchDaemon `install.sh` registers (`ai.litellm.relay.autoconfigure-desktop`) watches that directory and writes the plist back within seconds, so keep it installed on a managed fleet. Because the managed settings are OS-native, this is the surface you push through your MDM, see [mdm.md](mdm.md).

A static key is stored in that file in clear, readable by every local account like any managed preference (the app reads it as the developer). Keep static keys to proofs of concept and use SSO for a rollout.

## Usage

The developer only launches Claude Desktop. It opens on the gateway welcome screen ("Your organization has set up Claude to run through a custom inference gateway. No Claude.ai account needed.") and answers through the Gateway.

That welcome screen also offers "Or sign in with Claude.ai", and the app remembers that choice: a developer who picked it once keeps booting on Claude.ai even though the managed configuration is in place, until they quit the app and delete the `deploymentMode` key from `~/Library/Application Support/Claude-3p/claude_desktop_config.json`, or the fleet's MDM profile also sets `disableDeploymentModeChooser` to true, which hides the option.

![Claude Desktop gateway welcome screen](img/claude-desktop-welcome.png)

![Claude Desktop answering through the Gateway](img/claude-desktop-answer.png)

## Demo

Claude Desktop and Codex, both onboarded by Relay and answering through one LiteLLM Gateway with zero developer setup:

[▶ Watch the demo (mp4)](video/claude-desktop-codex-vscode-gateway-demo.mp4)
