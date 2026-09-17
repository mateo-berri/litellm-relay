use std::{env, fs, path::PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use crate::{
    ai_tools::gateway_credential::{ensure_gateway_credential, GatewayCredential, Renewal, SignIn},
    config::{load_settings, save_settings, RelaySettings},
};

/// Inputs for wiring Claude Desktop (third-party mode) to route through the
/// Gateway. Supplied by the MDM package or interactively; any field left unset
/// falls back to the saved Relay config.
///
/// When `oidc_client_id` and `oidc_issuer` are both set, the app is configured
/// for single sign-on: each developer signs in against the corporate IdP and
/// the resulting token is sent to the Gateway as the bearer credential, so no
/// provider key ever lands on the device. Otherwise a static Gateway credential
/// is written: the one passed as `api_key`, else one Relay exchanges the
/// developer's IdP token for, else the key saved in the Relay config.
#[derive(Debug, Default)]
pub struct OnboardDesktopParams {
    pub gateway_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_issuer: Option<String>,
    pub oidc_scopes: Option<String>,
    pub oidc_redirect_port: Option<u16>,
    /// Whether obtaining the Gateway credential may open a browser sign-in.
    /// The root autoconfigure daemon has no developer at the keyboard, so it
    /// only reuses an identity the developer already signed in with.
    pub allow_sign_in: bool,
    /// Suppress success output (used by autoconfigure, which prints its own
    /// summary). Standalone `relay onboard-claude-desktop` leaves this false.
    pub quiet: bool,
}

/// Writes `/etc/claude-desktop/managed-settings.json` so Claude Desktop routes
/// inference through the Gateway. The app reads this root-owned file on launch
/// (see the Anthropic "LLM gateway" third-party docs), switches into gateway
/// mode, and in SSO mode prompts the developer to sign in through their
/// browser on first use.
pub fn onboard_desktop(params: OnboardDesktopParams) -> Result<()> {
    let mut settings = load_settings()?;
    if let Some(gateway_url) = params.gateway_url {
        settings.gateway.url = gateway_url.trim_end_matches('/').to_string();
    }
    let explicit_api_key = params.api_key.as_deref().is_some_and(|key| !key.is_empty());
    if let Some(api_key) = params.api_key {
        settings.gateway.api_key = Some(api_key);
    }
    if let Some(model) = params.model {
        settings.claude.model = model;
    }

    let sso = match (&params.oidc_client_id, &params.oidc_issuer) {
        (Some(client_id), Some(issuer)) => Some(SsoConfig {
            client_id: client_id.clone(),
            issuer: issuer.clone(),
            scopes: params.oidc_scopes.clone(),
            redirect_port: params.oidc_redirect_port,
        }),
        (None, None) => None,
        _ => bail!("SSO requires both --oidc-client-id and --oidc-issuer"),
    };

    let sign_in = if params.allow_sign_in {
        SignIn::Allowed
    } else {
        SignIn::CachedOnly
    };
    let credential = resolve_credential(&settings, sso, explicit_api_key, || {
        ensure_gateway_credential(
            &settings,
            settings.claude.team.as_deref(),
            sign_in,
            Renewal::EveryRun,
        )
    })?;

    let document = build_managed_settings(&settings, &credential);
    let path = write_managed_settings(&document)?;
    save_settings(&settings)?;

    if !params.quiet {
        println!("Claude Desktop is wired to {}", settings.gateway.url);
        match &credential {
            DesktopCredential::Sso(sso) => {
                println!(
                    "Sign-in: OIDC issuer {} (client {})",
                    sso.issuer, sso.client_id
                );
                println!("Developers click \"Sign in to your organization\" on first launch.");
            }
            DesktopCredential::Static(StaticCredential::Exchanged(_)) => {
                println!("Credential: Gateway credential exchanged from your IdP sign-in")
            }
            DesktopCredential::Static(StaticCredential::Key(_)) => {
                println!("Credential: static Gateway API key")
            }
        }
        println!("Wrote {}", path.display());
        println!("Restart Claude Desktop to pick up the managed configuration.");
    }
    Ok(())
}

#[derive(Debug)]
struct SsoConfig {
    client_id: String,
    issuer: String,
    scopes: Option<String>,
    redirect_port: Option<u16>,
}

#[derive(Debug)]
enum StaticCredential {
    Key(String),
    Exchanged(String),
}

impl StaticCredential {
    fn secret(&self) -> &str {
        match self {
            Self::Key(secret) | Self::Exchanged(secret) => secret,
        }
    }
}

#[derive(Debug)]
enum DesktopCredential {
    Sso(SsoConfig),
    Static(StaticCredential),
}

// OIDC flags win because the developer signs in inside the app, and an explicit
// `--api-key` is the operator's choice. Otherwise a configured IdP means Relay
// exchanges the developer's sign-in, and a saved key covers a Gateway without
// the exchange or an exchange that failed.
fn resolve_credential(
    settings: &RelaySettings,
    sso: Option<SsoConfig>,
    explicit_api_key: bool,
    exchange: impl FnOnce() -> Result<GatewayCredential>,
) -> Result<DesktopCredential> {
    if let Some(sso) = sso {
        return Ok(DesktopCredential::Sso(sso));
    }
    let saved_key = settings
        .gateway
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty());
    if let Some(key) = saved_key.filter(|_| explicit_api_key) {
        return Ok(DesktopCredential::Static(StaticCredential::Key(
            key.to_string(),
        )));
    }
    if !settings.idp.authorize_url.trim().is_empty() {
        match exchange() {
            Ok(GatewayCredential::Issued(token)) => {
                return Ok(DesktopCredential::Static(StaticCredential::Exchanged(
                    token,
                )))
            }
            Ok(GatewayCredential::Unsupported) if saved_key.is_none() => bail!(
                "the gateway {} offers no IdP token exchange, so Claude Desktop needs a Gateway \
                 credential: pass --api-key for a static key, or --oidc-client-id and \
                 --oidc-issuer for single sign-on",
                settings.gateway.url
            ),
            Ok(GatewayCredential::Unsupported) => {}
            Err(error) if saved_key.is_none() => return Err(error),
            Err(error) => eprintln!(
                "could not exchange the IdP sign-in for a Gateway credential, keeping the \
                 saved Gateway key: {error:#}"
            ),
        }
    }
    match saved_key {
        Some(key) => Ok(DesktopCredential::Static(StaticCredential::Key(
            key.to_string(),
        ))),
        None => bail!(
            "Claude Desktop onboarding needs a Gateway credential: pass --api-key for a static \
             key, --oidc-client-id and --oidc-issuer for single sign-on, or onboard an IdP with \
             `relay onboard` so Relay can exchange your sign-in for one"
        ),
    }
}

/// Builds the top-level JSON object Claude Desktop reads from
/// `/etc/claude-desktop/managed-settings.json`. Keys match the Anthropic
/// third-party configuration reference exactly.
fn build_managed_settings(
    settings: &RelaySettings,
    credential: &DesktopCredential,
) -> Map<String, Value> {
    let mut root = Map::new();
    root.insert("inferenceProvider".into(), Value::String("gateway".into()));
    root.insert(
        "inferenceGatewayBaseUrl".into(),
        Value::String(settings.gateway.url.clone()),
    );
    root.insert(
        "inferenceGatewayAuthScheme".into(),
        Value::String("bearer".into()),
    );
    root.insert(
        "inferenceModels".into(),
        Value::Array(vec![Value::String(settings.claude.model.clone())]),
    );

    match credential {
        DesktopCredential::Sso(sso) => {
            root.insert(
                "inferenceCredentialKind".into(),
                Value::String("interactive".into()),
            );
            let mut oidc = Map::new();
            oidc.insert("clientId".into(), Value::String(sso.client_id.clone()));
            oidc.insert("issuer".into(), Value::String(sso.issuer.clone()));
            if let Some(scopes) = &sso.scopes {
                oidc.insert("scopes".into(), Value::String(scopes.clone()));
            }
            if let Some(port) = sso.redirect_port {
                oidc.insert("redirectPort".into(), json!(port));
            }
            root.insert("inferenceGatewayOidc".into(), Value::Object(oidc));
        }
        DesktopCredential::Static(credential) => {
            root.insert(
                "inferenceCredentialKind".into(),
                Value::String("static".into()),
            );
            root.insert(
                "inferenceGatewayApiKey".into(),
                Value::String(credential.secret().to_string()),
            );
        }
    }

    root
}

fn managed_settings_path() -> PathBuf {
    if let Ok(path) = env::var("CLAUDE_DESKTOP_MANAGED_SETTINGS") {
        return PathBuf::from(path);
    }
    PathBuf::from("/etc/claude-desktop/managed-settings.json")
}

fn write_managed_settings(document: &Map<String, Value>) -> Result<PathBuf> {
    let path = managed_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| managed_write_error(error, parent))?;
    }

    let serialized = serde_json::to_string_pretty(&Value::Object(document.clone()))?;
    fs::write(&path, format!("{serialized}\n"))
        .map_err(|error| managed_write_error(error, &path))?;
    Ok(path)
}

/// Maps a filesystem error on the managed `/etc/claude-desktop` path to a
/// concise, actionable message. Permission errors get a short "needs sudo" hint
/// (surfaced verbatim in the autoconfigure summary) instead of the raw
/// "Permission denied (os error 13)".
fn managed_write_error(error: std::io::Error, path: &std::path::Path) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow!("needs sudo (managed dir /etc/claude-desktop must be root-owned)")
    } else {
        anyhow::Error::new(error).context(format!("failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(url: &str, key: Option<&str>, model: &str) -> RelaySettings {
        let mut settings = RelaySettings::default();
        settings.gateway.url = url.into();
        settings.gateway.api_key = key.map(str::to_string);
        settings.claude.model = model.into();
        settings
    }

    fn settings_with_idp(key: Option<&str>) -> RelaySettings {
        let mut settings = settings_with("https://gw.corp", key, "claude-sonnet-4-5");
        settings.idp.authorize_url = "https://login.corp/authorize".into();
        settings
    }

    fn sso() -> SsoConfig {
        SsoConfig {
            client_id: "client-123".into(),
            issuer: "https://login.corp/v2.0".into(),
            scopes: None,
            redirect_port: Some(53180),
        }
    }

    fn static_secret(credential: &DesktopCredential) -> &str {
        match credential {
            DesktopCredential::Static(credential) => credential.secret(),
            DesktopCredential::Sso(_) => panic!("expected a static credential"),
        }
    }

    fn no_exchange() -> Result<GatewayCredential> {
        panic!("the exchange must not run")
    }

    #[test]
    fn should_write_static_gateway_config() {
        let settings = settings_with(
            "http://127.0.0.1:4000",
            Some("sk-test"),
            "claude-sonnet-4-5",
        );
        let credential = DesktopCredential::Static(StaticCredential::Key("sk-test".into()));
        let doc = build_managed_settings(&settings, &credential);

        assert_eq!(doc["inferenceProvider"], Value::String("gateway".into()));
        assert_eq!(
            doc["inferenceGatewayBaseUrl"],
            Value::String("http://127.0.0.1:4000".into())
        );
        assert_eq!(
            doc["inferenceCredentialKind"],
            Value::String("static".into())
        );
        assert_eq!(
            doc["inferenceGatewayApiKey"],
            Value::String("sk-test".into())
        );
        assert!(!doc.contains_key("inferenceGatewayOidc"));
        assert_eq!(doc["inferenceModels"], json!(["claude-sonnet-4-5"]));
    }

    #[test]
    fn should_write_the_exchanged_gateway_credential_as_the_static_key() {
        let settings = settings_with_idp(None);
        let credential =
            DesktopCredential::Static(StaticCredential::Exchanged("sk-exchanged".into()));
        let doc = build_managed_settings(&settings, &credential);

        assert_eq!(
            doc["inferenceCredentialKind"],
            Value::String("static".into())
        );
        assert_eq!(
            doc["inferenceGatewayApiKey"],
            Value::String("sk-exchanged".into())
        );
    }

    #[test]
    fn should_write_interactive_sso_config_without_api_key() {
        let settings = settings_with("https://gw.corp", Some("sk-secret"), "claude-sonnet-4-5");
        let doc = build_managed_settings(&settings, &DesktopCredential::Sso(sso()));

        assert_eq!(
            doc["inferenceCredentialKind"],
            Value::String("interactive".into())
        );
        assert!(
            !doc.contains_key("inferenceGatewayApiKey"),
            "SSO mode must not leak a static key onto the device"
        );
        let oidc = doc["inferenceGatewayOidc"].as_object().unwrap();
        assert_eq!(oidc["clientId"], Value::String("client-123".into()));
        assert_eq!(
            oidc["issuer"],
            Value::String("https://login.corp/v2.0".into())
        );
        assert_eq!(oidc["redirectPort"], json!(53180));
    }

    #[test]
    fn should_prefer_sso_over_every_other_credential() {
        let settings = settings_with_idp(Some("sk-saved"));

        let credential = resolve_credential(&settings, Some(sso()), true, no_exchange).unwrap();

        assert!(matches!(credential, DesktopCredential::Sso(_)));
    }

    #[test]
    fn should_prefer_an_explicit_api_key_over_the_exchange() {
        let settings = settings_with_idp(Some("sk-explicit"));

        let credential = resolve_credential(&settings, None, true, no_exchange).unwrap();

        assert_eq!(static_secret(&credential), "sk-explicit");
    }

    #[test]
    fn should_exchange_the_idp_sign_in_when_no_key_was_passed() {
        let settings = settings_with_idp(Some("sk-saved"));

        let credential = resolve_credential(&settings, None, false, || {
            Ok(GatewayCredential::Issued("sk-exchanged".into()))
        })
        .unwrap();

        assert_eq!(static_secret(&credential), "sk-exchanged");
    }

    #[test]
    fn should_fall_back_to_the_saved_key_when_the_gateway_has_no_exchange() {
        let settings = settings_with_idp(Some("sk-saved"));

        let credential = resolve_credential(&settings, None, false, || {
            Ok(GatewayCredential::Unsupported)
        })
        .unwrap();

        assert_eq!(static_secret(&credential), "sk-saved");
    }

    #[test]
    fn should_keep_the_saved_key_when_the_exchange_fails() {
        let settings = settings_with_idp(Some("sk-saved"));

        let credential = resolve_credential(&settings, None, false, || {
            bail!("no signed-in identity on this device")
        })
        .unwrap();

        assert_eq!(static_secret(&credential), "sk-saved");
    }

    #[test]
    fn should_surface_the_exchange_failure_when_no_key_is_saved() {
        let settings = settings_with_idp(None);

        let error = resolve_credential(&settings, None, false, || {
            bail!("no signed-in identity on this device")
        })
        .unwrap_err();

        assert!(error.to_string().contains("no signed-in identity"));
    }

    #[test]
    fn should_explain_when_the_gateway_has_no_exchange_and_nothing_else_is_configured() {
        let settings = settings_with_idp(None);

        let error = resolve_credential(&settings, None, false, || {
            Ok(GatewayCredential::Unsupported)
        })
        .unwrap_err();

        assert!(error.to_string().contains("offers no IdP token exchange"));
    }

    #[test]
    fn should_use_the_saved_key_without_an_idp() {
        let settings = settings_with("https://gw.corp", Some("sk-saved"), "claude-sonnet-4-5");

        let credential = resolve_credential(&settings, None, false, no_exchange).unwrap();

        assert_eq!(static_secret(&credential), "sk-saved");
    }

    #[test]
    fn should_require_some_credential() {
        let settings = settings_with("https://gw.corp", None, "claude-sonnet-4-5");

        let error = resolve_credential(&settings, None, false, no_exchange).unwrap_err();

        assert!(error.to_string().contains("needs a Gateway credential"));
    }
}
