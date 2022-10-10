use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use serde_json::json;
use spin_deploy::{
    auth::{AuthConnection, AuthError, DeviceFlowAuthenticator, PlatformConnection, TokenInfo},
    config::Config,
};

/// The client ID for Spin that a compatible target platform should recognize.
const SPIN_CLIENT_ID: &str = "583e63e9-461f-4fbe-a246-23e0fb1cad10";

/// Temporary login command for the Cloud.
#[derive(Parser, Debug)]
pub struct Login {
    #[clap(takes_value = false, long)]
    pub insecure: bool,

    // TODO: change the default URL.
    #[clap(default_value = "http://localhost:5309", long)]
    pub url: String,

    /// Get a device code.
    /// This is a hidden flag, as it is not indended for
    /// regular usage, but for tools that want to integrate with the flow.
    /// As a result, its output is in the JSON format, to ease parsing.
    #[clap(hidden = true, long, conflicts_with = "check-device-code")]
    pub get_device_code: bool,

    /// Check a device code.
    /// This is a hidden flag, as it is not indended for
    /// regular usage, but for tools that want to integrate with the flow.
    /// As a result, its output is in the JSON format, to ease parsing.
    #[clap(hidden = true, long, conflicts_with = "get-device-code")]
    pub check_device_code: Option<String>,

    /// Configuration file to write the authentication token.
    /// This lets users switch between multiple environments without having to
    /// re-authenticate.
    #[clap(long, env = "SPIN_AUTH")]
    pub config: Option<PathBuf>,

    /// Display the authentication status.
    #[clap(takes_value = false, long)]
    pub status: bool,
}

impl Login {
    pub async fn run(self) -> Result<()> {
        if self.status {
            let cfg = Config::new(self.config).await?;
            println!("Using configuration file: {}", cfg.auth_path.display());
            if !cfg.auth.is_token_valid()? {
                println!("Your authentication token for {} is no longer valid, please log in again using `spin login`", cfg.auth.platform_connection().url);
            }

            println!(
                "Authentication token for {} is valid until {}",
                cfg.auth.platform_connection().url,
                cfg.auth.platform_connection().token_info.expiration
            );

            match cfg.auth {
                AuthConnection::StandaloneRegistry(_, bc) => {
                    println!("Using standalone Bindle registry at {}", bc.url);
                    if bc.username.is_some() {
                        println!(
                            "Logged in to the Bindle registry as {}",
                            bc.username.unwrap()
                        );
                    }
                }

                _ => return Ok(()),
            };

            return Ok(());
        }

        let url = match self.url.strip_suffix('/') {
            Some(u) => u.to_string(),
            None => self.url.clone(),
        };

        let auth =
            DeviceFlowAuthenticator::new(url.clone(), self.insecure, SPIN_CLIENT_ID.to_string());

        // Functionality that non-interactive tools need in order to check a device code and obtain a token.
        if let Some(code) = self.check_device_code {
            // we check the device code, but since this is non-interactive,
            // we do not poll for a result, but return immediately.
            match check_device_code_with_timeout(&auth, code, 0, 0).await {
                Ok(token_info) => {
                    return print(&token_info, "cannot print token information".to_string())
                }
                // the consumer might want to take a different action depending
                // on whether the status is `waiting` or `failure`.
                Err(err) => match err {
                    AuthError::WaitingAuthorization => print(
                        &json!({"status": "waiting"}),
                        "cannot print waiting status".to_string(),
                    )?,
                    AuthError::Timeout => print(
                        &json!({"status": "timeout"}),
                        "cannot print timeout status".to_string(),
                    )?,
                    _ => {
                        return print(
                            &json!({"status": "unauthorized"}),
                            "cannot print failure status".to_string(),
                        )
                    }
                },
            }
        };

        let code = auth
            .get_device_code()
            .await
            .context("cannot get device code")?;

        // Functionality that non-interactive tools need in order to obtain a device code.
        if self.get_device_code {
            print(&code, "cannot print device code".to_string())?;
            return Ok(());
        }

        // If we made it this far, it means we need to perform the entire flow.
        println!(
            "Open {} in your browser, then introduce your one-time code: {}",
            code.verification_url
                .context("cannot get verification URL from server")?,
            code.user_code
                .context("cannot get one-time code from server")?
        );

        let token_info = check_device_code_with_timeout(
            &auth,
            code.device_code
                .expect("cannot get device code from server response"),
            15 * 60,
            5,
        )
        .await?;

        let cfg = Config::new_with_auth(
            self.config,
            AuthConnection::ProxiedRegistry(PlatformConnection {
                url,
                token_info,
                insecure: self.insecure,
            }),
        )
        .await?;
        cfg.commit().await?;

        Ok(())
    }
}

async fn check_device_code_with_timeout(
    auth: &DeviceFlowAuthenticator,
    code: String,
    timeout: u64,
    sleep: u64,
) -> Result<TokenInfo, AuthError> {
    let mut elapsed = 0;
    loop {
        if elapsed > timeout {
            return Err(AuthError::Timeout);
        }
        match auth.check_device_code(code.clone()).await {
            Ok(token_info) => return Ok(token_info),
            Err(err) => {
                if timeout > 0 {
                    println!(
                        "Waiting for device authorization, please follow the instructions to log in..."
                    );
                    tokio::time::sleep(Duration::from_secs(sleep)).await;
                    elapsed += sleep;
                    continue;
                } else {
                    // This is non-interactive mode, so we return the error.
                    return Err(err);
                }
            }
        }
    }
}

fn print<T: Serialize>(t: &T, err_ctx: String) -> Result<(), anyhow::Error> {
    println!("{}", serde_json::to_string_pretty(t).context(err_ctx)?);
    Ok(())
}
