use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::{client::IntoClientRequest, Message}};

use crate::commands::common::resolve_execution_mode;
use crate::execution::server::client::JupyterClient;

#[derive(Args)]
pub struct TerminalArgs {
    #[command(subcommand)]
    pub command: TerminalCommand,
}

#[derive(Subcommand)]
pub enum TerminalCommand {
    /// Execute a shell command in a remote Jupyter terminal
    Exec { command: String },
}

pub fn execute(args: TerminalArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let TerminalCommand::Exec { command } = args.command;
        let (server_url, credential) = match resolve_execution_mode(None, None)? {
            crate::execution::types::ExecutionMode::Remote { server_url, token } => (server_url, token),
            _ => anyhow::bail!("Terminal commands require a remote Jupyter connection"),
        };
        let client = JupyterClient::new(server_url, credential).await?;
        let name = format!("codex{}", uuid::Uuid::new_v4().simple());
        client.create_terminal(&name).await?;
        let result = run_terminal(&client, &name, &command).await;
        let _ = client.delete_terminal(&name).await;
        result
    })
}

async fn run_terminal(client: &JupyterClient, name: &str, command: &str) -> Result<()> {
    let marker = format!("__NB_DONE_{}__", uuid::Uuid::new_v4().simple());
    let ws_url = client.get_terminal_ws_url(name);
    let mut req = ws_url.into_client_request().context("Invalid terminal WebSocket URL")?;
    if let Some(cookie) = client.websocket_cookie() { req.headers_mut().insert("Cookie", cookie.parse()?); }
    let (mut ws, _) = connect_async(req).await.context("Failed to connect to Jupyter terminal WebSocket")?;
        ws.send(Message::Text(serde_json::to_string(&["stdin", &format!("{}; printf '\\n{}%s\\n' \"$?\"\n", command, marker)])?.into())).await?;
    let mut transcript = String::new();
    while let Some(message) = ws.next().await {
        let text = match message? { Message::Text(text) => text, _ => continue };
        let pair: Vec<Value> = serde_json::from_str(&text).context("Invalid terminal WebSocket message")?;
        if pair.len() != 2 { continue; }
        let output = pair[1].as_str().unwrap_or("");
        transcript.push_str(output);
        let first = transcript.find(&marker);
        let second = first.and_then(|pos| transcript[pos + marker.len()..].find(&marker).map(|next| pos + marker.len() + next));
        if let (Some(first), Some(second)) = (first, second) {
            let echoed_tail = &transcript[first + marker.len()..second];
            let command_output = echoed_tail.find('\n').map(|pos| &echoed_tail[pos + 1..]).unwrap_or("");
            print!("{}", command_output);
            let status_text = &transcript[second + marker.len()..];
            let status = status_text.lines().next().unwrap_or("0").trim().parse::<i32>().unwrap_or(1);
            if status != 0 { anyhow::bail!("remote command exited with status {status}"); }
            break;
        }
    }
    Ok(())
}
