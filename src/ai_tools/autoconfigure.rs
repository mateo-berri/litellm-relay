//! Auto-configuration: detect the AI tools installed on this device and wire
//! each one onto the Gateway in a single pass. This is what makes Relay "opt
//! out" instead of "opt in" — installing Relay routes every recognized tool
//! through the Gateway automatically, rather than requiring the operator to run
//! a separate onboard command per tool per machine.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use console::style;
use url::Url;

use crate::{
    ai_tools::{
        claude_cli::{onboard, OnboardParams},
        claude_desktop::{onboard_desktop, OnboardDesktopParams},
        codex::{onboard as onboard_codex, CodexOnboardParams},
        detect::{detect_all, AiTool, DetectContext, Detection},
    },
    config::load_settings,
    credential::{check_credential, expiry_state, CredentialCheck, ExpiryState, REENROLL_HINT},
};

const CREDENTIAL_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Overrides forwarded to each tool's onboarder. Every field is optional; when
/// unset the individual onboarders fall back to the saved Relay config, so a
/// managed `config.yaml` seeded by the MDM is enough to configure a device with
/// no flags at all.
#[derive(Debug, Default, Clone)]
pub struct AutoConfigureParams {
    pub gateway_url: Option<String>,
    pub authorize_url: Option<String>,
    pub team: Option<String>,
    /// Static Gateway key for tools without an IdP (Claude Desktop static mode,
    /// Codex static key).
    pub api_key: Option<String>,
    /// Codex-only: read the bearer key from this env var instead of the token
    /// helper hook.
    pub env_key: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_issuer: Option<String>,
    pub oidc_scopes: Option<String>,
    pub oidc_redirect_port: Option<u16>,
}

/// Whether the Gateway still accepts the static credential about to be written into tool configs.
#[derive(Clone, Debug, PartialEq)]
pub enum CredentialGate {
    NotStatic,
    Verified { expiry: ExpiryState },
    Rejected { detail: String },
    Unverifiable { gateway: String, detail: String },
}

/// Detect installed tools and onboard each one, continuing past any single
/// tool's failure so one misconfigured tool never blocks the rest. Returns an
/// error only if every detected tool failed to configure, or if the Gateway
/// does not accept the static credential that would be written.
///
/// `only` restricts the pass to specific tools (empty means every tool). This
/// lets the root-owned periodic agent handle just Claude Desktop (its managed
/// file lives under `/etc`) while the per-user agent handles the rest.
pub async fn autoconfigure(mut params: AutoConfigureParams, only: &[AiTool]) -> Result<()> {
    apply_credential_fallback(&mut params)?;
    let gate = credential_gate(&params).await?;
    autoconfigure_with(
        &DetectContext::from_env(),
        params,
        only,
        gate,
        &mut configure_tool,
    )
}

/// When the caller supplies no explicit credential and no IdP authorize URL is
/// configured, reuse the saved Gateway key so tools that accept a static
/// credential (Codex, Claude Desktop) still get wired up on non-SSO setups. A
/// configured IdP is always preferred and left untouched.
fn apply_credential_fallback(params: &mut AutoConfigureParams) -> Result<()> {
    if params.api_key.is_some() || params.env_key.is_some() || params.authorize_url.is_some() {
        return Ok(());
    }
    let settings = load_settings()?;
    if settings.idp.authorize_url.trim().is_empty() {
        params.api_key = settings
            .gateway
            .api_key
            .filter(|key| !key.trim().is_empty());
    }
    Ok(())
}

/// Verify the static key against the Gateway before it lands in any tool config.
async fn credential_gate(params: &AutoConfigureParams) -> Result<CredentialGate> {
    let Some(api_key) = &params.api_key else {
        return Ok(CredentialGate::NotStatic);
    };
    if params.authorize_url.is_some() {
        return Ok(CredentialGate::NotStatic);
    }
    let settings = load_settings()?;
    let gateway_url = params
        .gateway_url
        .clone()
        .unwrap_or_else(|| settings.gateway.url.clone());
    let http = reqwest::Client::builder()
        .timeout(CREDENTIAL_CHECK_TIMEOUT)
        .build()
        .expect("reqwest client configuration should be valid");
    let gate = match check_credential(&http, &gateway_url, api_key).await {
        CredentialCheck::Valid => {
            let expires_at = settings
                .gateway
                .expires_at
                .filter(|_| settings.gateway.api_key.as_deref() == Some(api_key.as_str()));
            CredentialGate::Verified {
                expiry: expiry_state(expires_at, Utc::now()),
            }
        }
        CredentialCheck::Rejected { detail, .. } => CredentialGate::Rejected { detail },
        CredentialCheck::Unverifiable { detail } => CredentialGate::Unverifiable {
            gateway: gateway_url,
            detail,
        },
    };
    Ok(gate)
}

/// Result of attempting to configure one detected tool.
struct Configured {
    tool: AiTool,
    outcome: Result<()>,
}

/// Testable core: detection context, credential gate, and per-tool configure
/// function are injected so unit tests can assert selection/reporting without
/// writing real tool config files or talking to a Gateway.
fn autoconfigure_with(
    ctx: &DetectContext,
    params: AutoConfigureParams,
    only: &[AiTool],
    gate: CredentialGate,
    configure: &mut dyn FnMut(AiTool, &AutoConfigureParams) -> Result<()>,
) -> Result<()> {
    let mut detected = detect_all(ctx);
    if !only.is_empty() {
        detected.retain(|detection| only.contains(&detection.tool));
    }
    if detected.is_empty() {
        println!(
            "No supported AI tools detected on this device. Relay will route them through the \
             Gateway automatically once Claude Code, Claude Desktop, or Codex is installed."
        );
        return Ok(());
    }

    println!(
        "{} {}",
        style("Auto-configuring AI tools →").bold(),
        style(gateway_host()).cyan().bold()
    );
    println!();

    match gate {
        CredentialGate::Rejected { detail } => {
            println!(
                "  {}  The Gateway rejected the stored credential: {detail}",
                style("✗").red().bold()
            );
            println!("     {REENROLL_HINT}");
            anyhow::bail!("Gateway credential rejected; no AI tool was configured");
        }
        CredentialGate::Unverifiable { gateway, detail } => {
            println!(
                "  {}  Could not verify the Gateway credential against {gateway}: {detail}. \
                 Leaving AI tool configs untouched.",
                style("!").yellow().bold()
            );
            anyhow::bail!("Gateway credential could not be verified; no AI tool was configured");
        }
        CredentialGate::Verified {
            expiry: ExpiryState::ExpiringSoon { at },
        } => {
            println!(
                "  {}  Gateway credential expires at {}. {REENROLL_HINT}",
                style("!").yellow().bold(),
                at.to_rfc3339()
            );
        }
        CredentialGate::NotStatic | CredentialGate::Verified { .. } => {}
    }

    let results: Vec<Configured> = detected
        .iter()
        .map(|Detection { tool, .. }| Configured {
            tool: *tool,
            outcome: configure(*tool, &params),
        })
        .collect();

    for configured in &results {
        let label = style(configured.tool.label()).bold();
        match &configured.outcome {
            Ok(()) => println!("  {}  {label}", style("✓").green().bold()),
            Err(error) => println!(
                "  {}  {label} {} {}",
                style("–").yellow().bold(),
                style("—").dim(),
                style(error).dim(),
            ),
        }
    }

    let failures = results
        .iter()
        .filter(|configured| configured.outcome.is_err())
        .count();
    let configured = results.len() - failures;

    println!();
    let summary = format!(
        "Configured {configured} of {} detected tools.",
        results.len()
    );
    if failures == 0 {
        println!("{}", style(summary).green().bold());
    } else {
        println!("{}", style(summary).yellow());
    }

    if failures > 0 && configured == 0 {
        anyhow::bail!("failed to configure any detected AI tool");
    }
    Ok(())
}

/// The Gateway host shown in the summary header. Loads the resolved settings and
/// extracts the URL host, falling back to the raw URL when it can't be parsed.
fn gateway_host() -> String {
    let raw = match load_settings() {
        Ok(settings) => settings.gateway.url,
        Err(_) => return "the Gateway".to_string(),
    };
    Url::parse(&raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or(raw)
}

/// Dispatch a single detected tool to its onboarder, forwarding overrides.
fn configure_tool(tool: AiTool, params: &AutoConfigureParams) -> Result<()> {
    match tool {
        AiTool::ClaudeCode => onboard(OnboardParams {
            gateway_url: params.gateway_url.clone(),
            authorize_url: params.authorize_url.clone(),
            team: params.team.clone(),
            model: None,
            api_key: params.api_key.clone(),
            quiet: true,
        }),
        AiTool::Codex => onboard_codex(CodexOnboardParams {
            gateway_url: params.gateway_url.clone(),
            authorize_url: params.authorize_url.clone(),
            team: params.team.clone(),
            model: None,
            env_key: params.env_key.clone(),
            api_key: params.api_key.clone(),
            quiet: true,
        }),
        AiTool::ClaudeDesktop => onboard_desktop(OnboardDesktopParams {
            gateway_url: params.gateway_url.clone(),
            api_key: params.api_key.clone(),
            model: None,
            oidc_client_id: params.oidc_client_id.clone(),
            oidc_issuer: params.oidc_issuer.clone(),
            oidc_scopes: params.oidc_scopes.clone(),
            oidc_redirect_port: params.oidc_redirect_port,
            quiet: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use chrono::DateTime;
    use std::{
        env, fs,
        path::{Path, PathBuf},
    };

    fn temp_home(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("relay-auto-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(home: &Path) -> DetectContext {
        DetectContext {
            home: home.to_path_buf(),
            path_dirs: Vec::new(),
            app_dirs: Vec::new(),
        }
    }

    fn static_key_params() -> AutoConfigureParams {
        AutoConfigureParams {
            api_key: Some("sk-static".into()),
            ..AutoConfigureParams::default()
        }
    }

    #[test]
    fn should_configure_only_detected_tools() {
        let home = temp_home("selected");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut seen: Vec<AiTool> = Vec::new();
        autoconfigure_with(
            &ctx(&home),
            AutoConfigureParams::default(),
            &[],
            CredentialGate::NotStatic,
            &mut |tool, _| {
                seen.push(tool);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(seen, vec![AiTool::ClaudeCode, AiTool::Codex]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_configure_only_the_requested_tools() {
        let home = temp_home("only");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut seen: Vec<AiTool> = Vec::new();
        autoconfigure_with(
            &ctx(&home),
            AutoConfigureParams::default(),
            &[AiTool::Codex],
            CredentialGate::NotStatic,
            &mut |tool, _| {
                seen.push(tool);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(seen, vec![AiTool::Codex]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_continue_when_one_tool_fails() {
        let home = temp_home("partial");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut attempted = 0;
        let result = autoconfigure_with(
            &ctx(&home),
            AutoConfigureParams::default(),
            &[],
            CredentialGate::NotStatic,
            &mut |tool, _| {
                attempted += 1;
                match tool {
                    AiTool::ClaudeCode => Err(anyhow!("no IdP configured")),
                    _ => Ok(()),
                }
            },
        );

        assert!(result.is_ok(), "one failure must not abort the run");
        assert_eq!(attempted, 2);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_error_when_all_tools_fail() {
        let home = temp_home("allfail");
        fs::create_dir_all(home.join(".codex")).unwrap();

        let result = autoconfigure_with(
            &ctx(&home),
            AutoConfigureParams::default(),
            &[],
            CredentialGate::NotStatic,
            &mut |_, _| Err(anyhow!("boom")),
        );

        assert!(result.is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_succeed_with_no_tools_detected() {
        let home = temp_home("none");
        let result = autoconfigure_with(
            &ctx(&home),
            AutoConfigureParams::default(),
            &[],
            CredentialGate::NotStatic,
            &mut |_, _| Ok(()),
        );
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_refuse_to_configure_any_tool_with_a_rejected_credential() {
        let home = temp_home("rejected");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut attempted = 0;
        let result = autoconfigure_with(
            &ctx(&home),
            static_key_params(),
            &[],
            CredentialGate::Rejected {
                detail: "Authentication Error - Expired Key".into(),
            },
            &mut |_, _| {
                attempted += 1;
                Ok(())
            },
        );

        let error = result.expect_err("a rejected credential must fail the run");
        assert!(error.to_string().contains("rejected"), "{error}");
        assert_eq!(attempted, 0, "no tool config may be written");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_leave_tools_untouched_when_the_credential_cannot_be_verified() {
        let home = temp_home("unverifiable");
        fs::create_dir_all(home.join(".codex")).unwrap();

        let mut attempted = 0;
        let result = autoconfigure_with(
            &ctx(&home),
            static_key_params(),
            &[],
            CredentialGate::Unverifiable {
                gateway: "http://127.0.0.1:1".into(),
                detail: "connection refused".into(),
            },
            &mut |_, _| {
                attempted += 1;
                Ok(())
            },
        );

        let error = result.expect_err("an unverifiable credential must fail the run");
        assert!(
            error.to_string().contains("could not be verified"),
            "{error}"
        );
        assert_eq!(attempted, 0, "no tool config may be written");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn should_configure_every_detected_tool_with_a_verified_credential() {
        let home = temp_home("verified");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        let expires_at = DateTime::parse_from_rfc3339("2026-09-22T22:27:58Z")
            .unwrap()
            .with_timezone(&Utc);

        for expiry in [
            ExpiryState::Unknown,
            ExpiryState::Ok { at: expires_at },
            ExpiryState::ExpiringSoon { at: expires_at },
            ExpiryState::Expired { at: expires_at },
        ] {
            let mut seen: Vec<AiTool> = Vec::new();
            autoconfigure_with(
                &ctx(&home),
                static_key_params(),
                &[],
                CredentialGate::Verified { expiry },
                &mut |tool, params| {
                    assert_eq!(params.api_key.as_deref(), Some("sk-static"));
                    seen.push(tool);
                    Ok(())
                },
            )
            .unwrap_or_else(|error| panic!("{expiry:?} must configure tools: {error}"));
            assert_eq!(seen, vec![AiTool::ClaudeCode, AiTool::Codex], "{expiry:?}");
        }
        let _ = fs::remove_dir_all(&home);
    }
}
