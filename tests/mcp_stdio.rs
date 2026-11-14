use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

fn isolate_config(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_torrenter"));
    cmd.arg("mcp")
        .env("XDG_CONFIG_HOME", dir)
        .env("APPDATA", dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env_remove("TORRENTER_TORBOX_KEY")
        .env_remove("TORRENTER_REALDEBRID_KEY")
        .env_remove("TORRENTER_ALLDEBRID_KEY")
        .env_remove("TORRENTER_PREMIUMIZE_KEY")
        .env_remove("TORRENTER_DEBRIDLINK_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

async fn spawn_mcp(dir: &Path) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = isolate_config(dir).spawn().expect("spawn torrenter mcp");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    (child, stdin, stdout)
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) {
    let mut line = serde_json::to_vec(value).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).await.unwrap();
    stdin.flush().await.unwrap();
}

async fn read_id(stdout: &mut BufReader<ChildStdout>, id: i64) -> Value {
    let deadline = timeout(Duration::from_secs(8), async {
        let mut line = String::new();
        loop {
            line.clear();
            let n = stdout.read_line(&mut line).await.unwrap();
            assert!(n > 0, "mcp stdout closed before id {id}");
            let parsed: Value = serde_json::from_str(line.trim()).unwrap_or_else(|error| {
                panic!("stdout is not JSON-RPC ({error}): {line}");
            });
            if parsed.get("id") == Some(&json!(id)) {
                return parsed;
            }
        }
    });
    deadline.await.expect("timed out waiting for MCP response")
}

async fn handshake(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>) -> Value {
    write_line(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "torrenter-test", "version": "0" }
            }
        }),
    )
    .await;
    let init = read_id(stdout, 1).await;
    write_line(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;
    init
}

async fn call_tool(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: i64,
    name: &str,
    arguments: Value,
) -> Value {
    write_line(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await;
    read_id(stdout, id).await
}

fn tool_text(response: &Value) -> String {
    let content = &response["result"]["content"];
    content
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or_else(|| panic!("missing tool text: {response}"))
        .to_string()
}

#[tokio::test]
async fn mcp_initialize_lists_tools_and_rejects_bad_urls() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, mut stdin, mut stdout) = spawn_mcp(dir.path()).await;
    let init = handshake(&mut stdin, &mut stdout).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "torrenter");

    write_line(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    let listed = read_id(&mut stdout, 2).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for name in [
        "banner",
        "dest",
        "queue_magnet",
        "queue_http",
        "jobs",
        "pause",
        "stop",
        "remove",
        "restart",
        "debrid_services",
    ] {
        assert!(names.contains(&name), "missing {name} in {names:?}");
    }

    let jobs = call_tool(&mut stdin, &mut stdout, 3, "jobs", json!({})).await;
    let payload: Value = serde_json::from_str(&tool_text(&jobs)).unwrap();
    assert_eq!(payload["jobs"], json!([]));

    let bad_magnet = call_tool(
        &mut stdin,
        &mut stdout,
        4,
        "queue_magnet",
        json!({"uri": "not-a-magnet"}),
    )
    .await;
    assert_eq!(bad_magnet["result"]["isError"], true);
    assert!(
        tool_text(&bad_magnet).contains("Invalid magnet URL!"),
        "{bad_magnet}"
    );

    let bad_http = call_tool(
        &mut stdin,
        &mut stdout,
        5,
        "queue_http",
        json!({"url": "ftp://example.com/a.bin"}),
    )
    .await;
    assert_eq!(bad_http["result"]["isError"], true);
    assert!(
        tool_text(&bad_http).contains("Invalid HTTP(S) URL!"),
        "{bad_http}"
    );

    let services = call_tool(&mut stdin, &mut stdout, 6, "debrid_services", json!({})).await;
    let payload: Value = serde_json::from_str(&tool_text(&services)).unwrap();
    assert_eq!(payload["services"][0]["id"], "torbox");
    assert_eq!(payload["services"][0]["configured"], false);

    let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10";
    let no_key = call_tool(
        &mut stdin,
        &mut stdout,
        7,
        "queue_magnet",
        json!({"uri": magnet, "debrid": "torbox"}),
    )
    .await;
    assert_eq!(no_key["result"]["isError"], true);
    assert!(
        tool_text(&no_key).contains("add a TorBox API key in settings first"),
        "{no_key}"
    );
    let unknown = call_tool(
        &mut stdin,
        &mut stdout,
        8,
        "queue_magnet",
        json!({"uri": magnet, "debrid": "nope"}),
    )
    .await;
    assert_eq!(unknown["result"]["isError"], true);

    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}

#[tokio::test]
async fn mcp_reports_env_keys_without_exposing_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = isolate_config(dir.path());
    cmd.env("TORRENTER_TORBOX_KEY", "env-secret-key-xyz");
    let mut child = cmd.spawn().expect("spawn torrenter mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    handshake(&mut stdin, &mut stdout).await;
    let services = call_tool(&mut stdin, &mut stdout, 2, "debrid_services", json!({})).await;
    let text = tool_text(&services);
    assert!(!text.contains("env-secret-key-xyz"), "{text}");
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(payload["services"][0]["configured"], true);
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    let _ = child.kill().await;
}
