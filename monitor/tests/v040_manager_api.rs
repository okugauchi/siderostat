//! H06: the GUI reads only the authenticated node-local Manager inventory.

use siderostat_monitor::manager_window::{ManagerCommand, ManagerEvent, execute_manager_command};
use siderostat_monitor::{client::MetricsClient, config::MonitorConfig};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};

fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(future)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let length = stream.read(&mut chunk).expect("read request");
        assert_ne!(length, 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..length]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let length = stream.read(&mut chunk).expect("read request body");
        assert_ne!(length, 0, "request body ended early");
        bytes.extend_from_slice(&chunk[..length]);
    }
    String::from_utf8(bytes).expect("request is UTF-8")
}

fn respond_json(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write response");
}

#[test]
fn inventory_fetch_uses_admin_bearer_and_decodes_only_sanitized_local_fields() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock admin listener");
    let address = listener.local_addr().expect("listener address");
    let response_body = serde_json::json!({
        "node_id": "local-node",
        "node_role": "coordinator",
        "source_commits": [{
            "receipt_id": format!("source-{}", "a".repeat(40)),
            "full_commit": "a".repeat(40),
            "main_proof": "refs/heads/main",
            "fetched_at": 1
        }],
        "artifacts": [{
            "id": format!("build-{}", "b".repeat(64)),
            "kind": "build",
            "digest": "b".repeat(64),
            "size": 1024,
            "verified": true,
            "source_commit": "a".repeat(40),
            "role": "ds4-server",
            "catalog_id": null
        }],
        "profiles": [],
        "node_readiness": {"ready": false, "reason": "no staged profile"},
        "active_digest": null,
        "previous_digest": null,
        "activation_phase": null
    })
    .to_string();
    let server_body = response_body.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = [0; 8192];
        let length = stream.read(&mut request).expect("read request");
        let request = String::from_utf8_lossy(&request[..length]).to_string();
        let bytes = server_body.as_bytes();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            bytes.len(),
            server_body
        )
        .expect("write response");
        request
    });

    let config = MonitorConfig {
        admin_listen: format!("http://{address}"),
        admin_token: Some("manager-test-bearer".to_string()),
        ..MonitorConfig::default()
    };
    let client = MetricsClient::new(&config).expect("client");
    let inventory = block_on(client.fetch_manager_inventory()).expect("inventory");
    let request = server.join().expect("server task");

    assert!(request.starts_with("GET /manager/inventory "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer manager-test-bearer"),
        "admin bearer missing: {request}"
    );
    assert_eq!(inventory.node_id, "local-node");
    assert_eq!(inventory.node_role.as_deref(), Some("coordinator"));
    assert_eq!(inventory.artifacts.len(), 1);
    let serialized = serde_json::to_string(&inventory).expect("serialize DTO");
    for forbidden in ["path", "url", "secret", "manager-test-bearer"] {
        assert!(
            !serialized.to_ascii_lowercase().contains(forbidden),
            "unexpected field/value {forbidden}: {serialized}"
        );
    }
}

#[test]
fn build_and_stage_commands_encode_only_local_inventory_identities() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock admin listener");
    let address = listener.local_addr().expect("listener address");
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for id in ["build-job", "stage-job"] {
            let (mut stream, _) = listener.accept().expect("accept request");
            requests.push(read_request(&mut stream));
            respond_json(&mut stream, &format!(r#"{{"id":"{id}"}}"#));
        }
        requests
    });
    let config = MonitorConfig {
        admin_listen: format!("http://{address}"),
        admin_token: Some("manager-test-bearer".to_string()),
        ..MonitorConfig::default()
    };
    let client = MetricsClient::new(&config).expect("client");
    let commit = "a".repeat(40);
    let build_id = format!("build-{}", "b".repeat(64));
    let model_id = format!("model-{}", "c".repeat(64));

    let build_event = block_on(execute_manager_command(
        &client,
        ManagerCommand::Build {
            source_receipt_id: format!("source-{commit}"),
            role: "ds4-server".to_string(),
        },
    ));
    let stage_event = block_on(execute_manager_command(
        &client,
        ManagerCommand::Stage {
            build_artifact_id: build_id.clone(),
            model_artifact_id: model_id.clone(),
        },
    ));
    let requests = server.join().expect("server task");

    assert_eq!(
        build_event,
        ManagerEvent::Submitted {
            kind: "build".to_string(),
            id: "build-job".to_string(),
        }
    );
    assert_eq!(
        stage_event,
        ManagerEvent::Submitted {
            kind: "stage".to_string(),
            id: "stage-job".to_string(),
        }
    );
    let build_body: serde_json::Value =
        serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).expect("build body"))
            .expect("build JSON");
    let stage_body: serde_json::Value =
        serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).expect("stage body"))
            .expect("stage JSON");
    assert_eq!(
        build_body,
        serde_json::json!({"kind":"build", "payload_key":format!("source-{commit}:ds4-server")})
    );
    assert_eq!(
        stage_body,
        serde_json::json!({"kind":"stage", "payload_key":format!("{build_id}:{model_id}")})
    );
    for request in requests {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer manager-test-bearer"),
            "admin bearer missing: {request}"
        );
    }
}
