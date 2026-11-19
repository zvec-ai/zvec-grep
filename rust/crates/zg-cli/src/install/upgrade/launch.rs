use std::path::PathBuf;

use serde_json::Value;

use super::{Launch, known_stdio_args};

/// Startup settings retained by migration, without token values or unrelated environment data.
#[derive(Debug)]
pub enum IntegrationLaunch {
    Stdio {
        home: Option<PathBuf>,
        listen: Option<String>,
        toolset: Option<String>,
        token_file: Option<PathBuf>,
        token_environment: bool,
    },
    Http,
    Unknown,
}

pub(super) fn record(server: &Value, launch: &Launch, output: &mut Vec<IntegrationLaunch>) {
    if matches!(launch, Launch::Ignore) {
        return;
    }
    if server
        .get("url")
        .or_else(|| server.get("httpUrl"))
        .is_some()
    {
        output.push(IntegrationLaunch::Http);
        return;
    }
    let args = server
        .get("command")
        .and_then(Value::as_array)
        .and_then(|args| args.get(1..))
        .or_else(|| {
            server
                .get("args")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
        });
    let Some(mut args) = args else {
        output.push(IntegrationLaunch::Unknown);
        return;
    };
    if matches!(
        launch,
        Launch::Update {
            script: Some(_),
            ..
        }
    ) {
        args = &args[1..];
    }
    if !known_stdio_args(args) || matches!(launch, Launch::Manual) {
        output.push(IntegrationLaunch::Unknown);
        return;
    }
    let option = |flag: &str| {
        args[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .rev()
            .find(|pair| pair[0] == flag)
            .and_then(|pair| pair[1].as_str())
            .map(str::to_owned)
    };
    let environment = |name: &str| {
        server
            .get("env")
            .and_then(|env| env.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let home = option("--home").or_else(|| environment("ZVEC_GREP_HOME"));
    let token_file = option("--token-file").or_else(|| environment("ZVEC_GREP_SERVER_TOKEN_FILE"));
    output.push(IntegrationLaunch::Stdio {
        home: home.map(PathBuf::from),
        listen: option("--listen"),
        toolset: option("--mcp-toolset"),
        token_file: token_file.map(PathBuf::from),
        token_environment: server
            .get("env")
            .and_then(|env| env.get("ZVEC_GREP_SERVER_TOKEN"))
            .is_some()
            || server
                .get("env_vars")
                .and_then(Value::as_array)
                .is_some_and(|vars| vars.iter().any(|name| name == "ZVEC_GREP_SERVER_TOKEN")),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_stdio_keeps_startup_settings_without_retaining_token_values() {
        let server = json!({"command":"node", "args":["package/dist/cli/index.js", "server", "--stdio", "--listen", "127.0.0.1:9123", "--mcp-toolset", "full"],
            "env":{"ZVEC_GREP_HOME":"data", "ZVEC_GREP_SERVER_TOKEN_FILE":"credential", "ZVEC_GREP_SERVER_TOKEN":"secret-not-for-output"}});
        let mut launches = Vec::new();
        record(
            &server,
            &Launch::Update {
                array: "args",
                action_index: 1,
                script: Some("package/dist/cli/index.js".to_owned()),
                replace_executable: true,
            },
            &mut launches,
        );
        assert!(
            matches!(&launches[0], IntegrationLaunch::Stdio { home:Some(home), listen:Some(listen), toolset:Some(toolset), token_file:Some(token_file), token_environment:true }
            if home == &PathBuf::from("data") && listen == "127.0.0.1:9123" && toolset == "full" && token_file == &PathBuf::from("credential"))
        );
        assert!(!format!("{launches:?}").contains("secret-not-for-output"));
    }

    #[test]
    fn http_and_unrecognized_arguments_cannot_claim_automatic_startup() {
        let mut launches = Vec::new();
        record(
            &json!({"url":"http://127.0.0.1:9123/mcp"}),
            &Launch::Compatible,
            &mut launches,
        );
        record(
            &json!({"command":"zg", "args":["--server", "--stdio", "--unknown"]}),
            &Launch::Compatible,
            &mut launches,
        );
        assert!(matches!(
            launches.as_slice(),
            [IntegrationLaunch::Http, IntegrationLaunch::Unknown]
        ));
    }
}
