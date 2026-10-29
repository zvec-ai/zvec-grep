//! Exercise the production terminal prompts without saving credentials or grants.
//!
//! This is a presentation fixture, not the `zg` command. Its only effects are
//! terminal input/output; a returned credential is discarded without printing it.

use std::{error::Error, process::ExitCode};

use clap::{Parser, ValueEnum};
use zg_cli::{
    ColorMode, SetupAuthorizationDecision, prompt_setup_authorization, prompt_setup_credential,
};
use zg_engine::authorization::IndexAuthorization;

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Credential,
    Authorization,
}

#[derive(Parser)]
struct Arguments {
    #[arg(long, value_enum)]
    mode: Mode,
    #[arg(long)]
    no_color: bool,
}

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let arguments = Arguments::parse();
    let color = if arguments.no_color {
        ColorMode::Never
    } else {
        ColorMode::Auto
    };
    let cancelled = match arguments.mode {
        Mode::Credential => {
            if prompt_setup_credential("qwen/text-embedding-v4", color)?.is_some() {
                println!("Decision: credential entered (fixture only; not saved)");
                false
            } else {
                println!("Decision: cancelled (fixture only; nothing saved)");
                true
            }
        }
        Mode::Authorization => {
            let target = IndexAuthorization {
                root: "/workspace/review-demo".into(),
                workspace_roots: vec!["/workspace/review-demo".into()],
                model: "qwen/text-embedding-v4".into(),
                endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings".into(),
                endpoint_host: "dashscope.aliyuncs.com".into(),
            };
            let decision = prompt_setup_authorization(&target, true, color)?;
            let label = match decision {
                SetupAuthorizationDecision::Once => "once",
                SetupAuthorizationDecision::Workspace => "workspace",
                SetupAuthorizationDecision::Back => "back",
                SetupAuthorizationDecision::Cancel => "cancelled",
            };
            println!("Decision: {label} (fixture only; no authorization stored)");
            decision == SetupAuthorizationDecision::Cancel
        }
    };
    Ok(if cancelled {
        ExitCode::from(130)
    } else {
        ExitCode::SUCCESS
    })
}
