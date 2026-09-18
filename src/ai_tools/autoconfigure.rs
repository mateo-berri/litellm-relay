//! Auto-configuration: detect the AI tools installed on this device and wire
//! each one onto the Gateway in a single pass. This is what makes Relay "opt
//! out" instead of "opt in" — installing Relay routes every recognized tool
//! through the Gateway automatically, rather than requiring the operator to run
//! a separate onboard command per tool per machine.

use anyhow::Result;
use console::style;
use url::Url;

use crate::{
    ai_tools::{
        claude_cli::{onboard, OnboardParams},
        claude_desktop::{onboard_desktop, OnboardDesktopParams},
        codex::{onboard as onboard_codex, CodexOnboardParams},
        detect::{detect_all, AiTool, DetectContext, Detection},
    },
    config::{load_settings, IdpOverrides, RelaySettings},
};

/// Overrides forwarded to each tool's onboarder. Every field is optional; when
/// unset the individual onboarders fall back to the saved Relay config, so a
/// managed `config.yaml` seeded by the MDM is enough to configure a device with
/// no flags at all.
#[derive(Debug, Default, Clone)]
pub struct AutoConfigureParams {
    pub gateway_url: Option<String>,
    pub team: Option<String>,
    /// Static Gateway key for tools without an IdP (Claude Desktop static mode,
    /// Codex static key).
    pub api_key: Option<String>,
    /// Codex-only: read the bearer key from this env var instead of the token
    /// helper hook.
    pub env_key: Option<String>,
    pub idp: IdpOverrides,
}

/// Detect installed tools and onboard each one, continuing past any single
/// tool's failure so one misconfigured tool never blocks the rest. Returns an
/// error only if every detected tool failed to configure.
///
/// `only` restricts the pass to specific tools (empty means every tool). This
/// lets the root-owned periodic agent handle just Claude Desktop (its managed
/// file lives under `/etc`) while the per-user agent handles the rest.
pub fn autoconfigure(mut params: AutoConfigureParams, only: &[AiTool]) -> Result<()> {
    apply_credential_fallback(&mut params)?;
    autoconfigure_with(
        &DetectContext::from_env(),
        params,
        only,
        &mut configure_tool,
    )
}

/// When the caller supplies no explicit credential and the IdP that results
/// from the saved config plus the overrides is incomplete, reuse the saved
/// Gateway key so tools that accept a static credential (Codex, Claude
/// Desktop) still get wired up on non-SSO setups. A configured IdP is always
/// preferred and left untouched.
fn apply_credential_fallback(params: &mut AutoConfigureParams) -> Result<()> {
    if params.api_key.is_some() || params.env_key.is_some() {
        return Ok(());
    }
    params.api_key = saved_key_fallback(&params.idp, &load_settings()?);
    Ok(())
}

fn saved_key_fallback(overrides: &IdpOverrides, settings: &RelaySettings) -> Option<String> {
    let mut idp = settings.idp.clone();
    idp.apply(overrides);
    if idp.is_configured() {
        return None;
    }
    settings
        .gateway
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
}

/// Result of attempting to configure one detected tool.
struct Configured {
    tool: AiTool,
    outcome: Result<()>,
}

/// Testable core: detection context and per-tool configure function are
/// injected so unit tests can assert selection/reporting without writing real
/// tool config files.
fn autoconfigure_with(
    ctx: &DetectContext,
    params: AutoConfigureParams,
    only: &[AiTool],
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
            team: params.team.clone(),
            model: None,
            api_key: params.api_key.clone(),
            idp: params.idp.clone(),
            quiet: true,
        }),
        AiTool::Codex => onboard_codex(CodexOnboardParams {
            gateway_url: params.gateway_url.clone(),
            team: params.team.clone(),
            model: None,
            env_key: params.env_key.clone(),
            api_key: params.api_key.clone(),
            idp: params.idp.clone(),
            quiet: true,
        }),
        AiTool::ClaudeDesktop => onboard_desktop(OnboardDesktopParams {
            gateway_url: params.gateway_url.clone(),
            api_key: params.api_key.clone(),
            model: None,
            oidc_client_id: params.idp.client_id.clone(),
            oidc_issuer: params.idp.issuer.clone(),
            oidc_scopes: params.idp.scopes.clone(),
            oidc_redirect_port: params.idp.redirect_port,
            quiet: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
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
            &mut |_, _| Ok(()),
        );
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&home);
    }

    fn settings(issuer: &str, client_id: &str, api_key: Option<&str>) -> RelaySettings {
        let mut settings = RelaySettings::default();
        settings.idp.issuer = issuer.into();
        settings.idp.client_id = client_id.into();
        settings.gateway.api_key = api_key.map(str::to_string);
        settings
    }

    #[test]
    fn should_fall_back_to_the_saved_key_only_without_a_usable_idp() {
        let saved_key = Some("sk-saved");
        let no_overrides = IdpOverrides::default();

        assert_eq!(
            saved_key_fallback(&no_overrides, &settings("", "", saved_key)),
            Some("sk-saved".into())
        );
        assert_eq!(
            saved_key_fallback(&no_overrides, &settings("", "", Some("  "))),
            None
        );
        assert_eq!(
            saved_key_fallback(
                &no_overrides,
                &settings("https://login.example.com", "client", saved_key)
            ),
            None
        );
    }

    #[test]
    fn should_judge_the_idp_after_applying_the_overrides() {
        let saved_key = Some("sk-saved");
        let issuer_only = IdpOverrides {
            issuer: Some("https://login.example.com".into()),
            ..IdpOverrides::default()
        };
        let client_only = IdpOverrides {
            client_id: Some("client".into()),
            ..IdpOverrides::default()
        };

        assert_eq!(
            saved_key_fallback(&issuer_only, &settings("", "", saved_key)),
            Some("sk-saved".into())
        );
        assert_eq!(
            saved_key_fallback(&issuer_only, &settings("", "client", saved_key)),
            None
        );
        assert_eq!(
            saved_key_fallback(
                &client_only,
                &settings("https://login.example.com", "", saved_key)
            ),
            None
        );
    }
}
