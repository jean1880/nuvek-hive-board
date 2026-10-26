//! End to end over stdio: with the server unreachable, write tools return an error result and the
//! binary creates nothing under HOME (no ~/.local/share/hive-board, no fallback database).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn call_line(id: u32, name: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    )
}

#[test]
fn dead_server_errors_and_writes_no_files() {
    let home = std::env::temp_dir().join(format!("hive-mcp-test-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_hive-mcp"))
        .env("HOME", &home)
        .env("HIVE_SERVER_URL", "http://127.0.0.1:9")
        .env("HIVE_API_TOKEN", "test-token-not-real")
        .env("NUVEK_LOG_SINK", "stderr")
        .env_remove("NEO4J_PASSWORD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    let inited = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let post = call_line(
        2,
        "board_post_message",
        r#"{"topic":"some-topic","author":"t","content":"hello","kind":"finding","severity":"NICE"}"#,
    );
    let create = call_line(
        3,
        "board_create_topic",
        r#"{"name":"some-topic","title":"t","author":"t"}"#,
    );
    for l in [init, inited, &post, &create] {
        writeln!(stdin, "{l}").unwrap();
    }
    stdin.flush().unwrap();

    let (mut got_post, mut got_create) = (None, None);
    while got_post.is_none() || got_create.is_none() {
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("no response from hive-mcp");
        if line.contains(r#""id":2"#) {
            got_post = Some(line);
        } else if line.contains(r#""id":3"#) {
            got_create = Some(line);
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    for resp in [got_post.unwrap(), got_create.unwrap()] {
        assert!(resp.contains(r#""isError":true"#), "{resp}");
        assert!(
            resp.contains("hive-board unreachable at http://127.0.0.1:9"),
            "{resp}"
        );
        assert!(resp.contains("nothing was written"), "{resp}");
        assert!(!resp.contains("test-token-not-real"), "{resp}");
    }

    assert!(!home.join(".local/share/hive-board").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&home).unwrap().collect();
    assert!(
        leftovers.is_empty(),
        "binary wrote under HOME: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
