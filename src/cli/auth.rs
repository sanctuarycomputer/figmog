//! Interactive input stays in the CLI; credential resolution and storage
//! live in crate::auth so the MCP server uses the same saved token.

use std::io::{IsTerminal, Read};

use clap::Subcommand;

use super::write_json;

#[derive(Subcommand)]
pub(super) enum AuthCommand {
    /// Save a personal access token in the system credential store.
    Login {
        /// Read the token from stdin instead of a hidden terminal prompt.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Show the active credential source without revealing the token.
    Status,
    /// Remove the saved token (does not unset FIGMA_TOKEN).
    Logout,
}

pub(super) fn run(command: &AuthCommand) -> Result<(), String> {
    let result = match command {
        AuthCommand::Login { token_stdin } => {
            let token = if *token_stdin {
                let mut token = String::new();
                std::io::stdin()
                    .take(crate::auth::MAX_TOKEN_BYTES as u64 + 3)
                    .read_to_string(&mut token)
                    .map_err(|_| "could not read token from stdin".to_string())?;
                if token.len() > crate::auth::MAX_TOKEN_BYTES + 2 {
                    return Err(
                        "token input is too long (maximum 4096 bytes plus a newline)".into(),
                    );
                }
                token
            } else {
                if !std::io::stdin().is_terminal() {
                    return Err(
                        "auth login needs a terminal; use --token-stdin to read a token from stdin"
                            .into(),
                    );
                }
                rpassword::prompt_password("Figma personal access token: ")
                    .map_err(|_| "could not read token from terminal".to_string())?
            };
            crate::auth::login(&token)?
        }
        AuthCommand::Status => crate::auth::status()?,
        AuthCommand::Logout => crate::auth::logout()?,
    };
    write_json(&result)
}
