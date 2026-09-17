# Claude Code onboarding

Relay onboards Claude Code onto your LiteLLM AI Gateway with zero manual setup. Employees never receive a provider API key and never export environment variables. Their corporate identity authenticates each request, and the Gateway maps that identity to a per-user virtual key with its own budget, model access, and spend tracking.

## How it works

An admin enables JWT auth on the Gateway once, and from then on onboarding a device is a single `relay onboard` call. The MDM package (Jamf/Intune) installs Claude Code from your internal registry (npm/Homebrew via JFrog) alongside Relay, then runs `relay onboard`, which writes `~/.claude/settings.json` so Claude Code points at the Gateway and pulls its bearer token from Relay's token helper

When the developer runs `claude`, Relay signs them in through the corporate IdP on first use and hands Claude Code a short-lived bearer token. The Gateway validates it, maps it to the developer's virtual key, enforces budget and limits, logs spend, and forwards upstream. No provider key ever touches the device, and offboarding is removing the identity from the SSO group, after which its tokens stop validating

## Usage

`relay onboard` writes the Claude Code settings pointing at the Gateway:

![relay onboard writing Claude settings](img/claude-onboard.png)

On first use Relay opens the corporate IdP sign-in in the browser (a local mock IdP is shown here; in production this is your Okta, Entra, or Google tenant):

![corporate IdP sign-in](img/claude-idp-signin.png)

After sign-in, Claude Code answers through the Gateway with no key on the device:

![Claude Code answering through the Gateway](img/claude-code-answer.png)

The Gateway auto-registers a per-user virtual key from the SSO identity and tracks spend by user and team:

![auto-registered per-user virtual keys](img/claude-virtual-keys.png)

## Commands

`relay onboard` wires Claude Code to the Gateway and records the IdP authorize URL, team, and model:

```bash
relay onboard \
  --gateway-url https://gateway.yourco.com \
  --authorize-url https://login.yourco.com/authorize \
  --team engineering \
  --model claude-sonnet-4-5
```

`relay claude-token` is what Claude Code's `apiKeyHelper` calls. It prints a Gateway credential for the configured team to stdout and nothing else; diagnostics go to stderr. Behind it, Relay starts a browser sign-in when the identity token is missing or within a minute of expiry, registers once with the Gateway's authorization server (`/.well-known/litellm-cli-auth`, then `/register`), exchanges the identity token for a Gateway credential at `/token` (the RFC 8693 token-exchange grant, with the team in the `x-litellm-team-id` header), and renews that credential with its refresh token ten minutes before it expires, without a new sign-in. When the refresh token has lapsed, Relay exchanges the identity token again, and when a renewal fails (the Gateway cannot be reached, or no sign-in is possible) it keeps serving the cached credential until that expires. The identity token and the refresh token are only ever posted to the Gateway URL Relay was onboarded with, whatever host the discovery document advertises, and those posts never follow a redirect. Concurrent hook runs take turns on a lock file, so the single-use refresh token is spent once. A Gateway that has no authorization server gets the identity token itself, as before, with a notice on stderr.

## Generated settings

```json
{
  "apiKeyHelper": "relay claude-token",
  "env": {
    "ANTHROPIC_BASE_URL": "https://gateway.yourco.com",
    "ANTHROPIC_CUSTOM_HEADERS": "x-litellm-team: engineering",
    "ANTHROPIC_MODEL": "claude-sonnet-4-5"
  }
}
```

No provider API key is written to the device. The identity token is cached under `~/.litellm-relay/identity-token.json` and the Gateway credential (one per Gateway and team, with its refresh token) under `~/.litellm-relay/gateway-credentials.json`, both with `0600` permissions on Unix.

## Gateway configuration

The headline auth mode is JWT with `auto_register`. The Gateway validates the identity token Relay presents at `/token` against your IdP's JWKS, maps its claims to a per-user virtual key and team, and issues the Gateway credential the tools then send as their bearer. The same configuration validates the identity token sent directly by an older Relay:

```yaml
general_settings:
  enable_jwt_auth: True
  litellm_jwtauth:
    user_id_jwt_field: "sub"
    user_id_upsert: True
    team_id_jwt_field: "team_id"
    team_id_upsert: True
    virtual_key_claim_field: "email"
    unregistered_jwt_client_behavior: "auto_register"
```

```bash
JWT_PUBLIC_KEY_URL="https://login.yourco.com/.well-known/jwks.json"
JWT_ISSUER="https://login.yourco.com"
JWT_AUDIENCE="litellm-gateway"
```

## Production versus demo IdP

In production, `--authorize-url` points at your corporate IdP's OIDC authorize endpoint (Okta, Entra, Google, and similar). The screenshots in the README use a local mock IdP for demonstration only; it is not part of a deployment.

## MDM rollout

The MDM package installs Claude Code from your internal registry and runs `relay onboard` with your Gateway URL, IdP authorize URL, and default team. Everything else follows the standard Relay rollout in [mdm.md](mdm.md): package the repo, deploy to the pilot scope, then broaden through Jamf or Intune. Because the settings file contains no provider key and the credential is obtained at runtime from the developer's IdP sign-in, the same package is safe to push fleet-wide.
