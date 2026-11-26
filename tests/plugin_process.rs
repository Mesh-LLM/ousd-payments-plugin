#![cfg(all(unix, feature = "offline-fixture"))]

use mesh_llm_plugin::{LocalStream, PROTOCOL_VERSION, proto, read_envelope, write_envelope};
use proto::envelope::Payload;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    net::UnixListener,
    process::{Child, Command},
    time::timeout,
};

async fn send(stream: &mut LocalStream, id: u64, payload: Payload) {
    write_envelope(
        stream,
        &proto::Envelope {
            protocol_version: PROTOCOL_VERSION,
            plugin_id: "tempo".into(),
            request_id: id,
            payload: Some(payload),
        },
    )
    .await
    .unwrap();
}
async fn receive(stream: &mut LocalStream) -> proto::Envelope {
    timeout(Duration::from_secs(10), read_envelope(stream))
        .await
        .unwrap()
        .unwrap()
}
async fn launch(binary: &str, root: &Path) -> (Child, LocalStream) {
    let socket = root.join("host.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(binary)
        .env("MESH_LLM_PLUGIN_NAME", "tempo")
        .env("MESH_LLM_PLUGIN_ENDPOINT", &socket)
        .env("MESH_LLM_PLUGIN_TRANSPORT", "unix")
        .env("MESH_LLM_OFFLINE_CHAIN_PATH", root.join("chain.sqlite"))
        .env("MESH_LLM_OFFLINE_ADDRESS", format!("0x{}", "11".repeat(20)))
        .env_remove("TEMPO_FIXTURE_NO_VERIFIER")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    (child, LocalStream::Unix(stream))
}
async fn initialize(stream: &mut LocalStream, root: &Path) {
    initialize_config(stream, root, json!({}), true).await;
}
async fn initialize_config(
    stream: &mut LocalStream,
    root: &Path,
    extra: Value,
    expect_publication: bool,
) {
    let config = root.join("payments.json");
    let mut settings = json!({"data_dir":root.join("payments")});
    settings
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
    send(
        stream,
        1,
        Payload::InitializeRequest(proto::InitializeRequest {
            host_protocol_version: PROTOCOL_VERSION,
            host_version: "test".into(),
            host_info_json: "{}".into(),
            mesh_visibility: proto::MeshVisibility::Private.into(),
            host_capabilities: vec![
                mesh_llm_plugin::peer_evidence::HOST_CAPABILITY.into(),
                mesh_llm_plugin::host_capabilities::OPENAI_EXCHANGE.into(),
                mesh_llm_plugin::host_capabilities::MODEL_REQUIREMENTS.into(),
            ],
            settings_json: json!({"config_path":config}).to_string(),
        }),
    )
    .await;
    let response = receive(stream).await;
    let Some(Payload::InitializeResponse(initialized)) = response.payload else {
        panic!("initialization failed")
    };
    let manifest = initialized.manifest.unwrap();
    assert_eq!(
        manifest
            .operations
            .iter()
            .map(|op| op.name.as_str())
            .collect::<Vec<_>>(),
        ["control", "verify_peer_evidence"]
    );
    let verifier = manifest
        .peer_evidence_verifier
        .as_ref()
        .expect("provider verifier declaration missing");
    assert_eq!(verifier.contract_version, 1);
    assert_eq!(verifier.namespace, "tempo.bond.v1");
    assert_eq!(verifier.handler, "verify_peer_evidence");
    assert!(
        manifest
            .grant_requests
            .is_none_or(|g| g.operation_bindings.is_empty())
    );
    assert!(
        manifest
            .mesh_event_subscriptions
            .iter()
            .any(|subscription| subscription.kind == proto::mesh_event::Kind::MeshIdUpdated as i32)
    );
    assert!(initialized.capabilities.is_empty());
    send(
        stream,
        0,
        Payload::MeshEvent(proto::MeshEvent {
            kind: proto::mesh_event::Kind::MeshIdUpdated as i32,
            local_peer_id: "aa".repeat(32),
            mesh_id: "fixture".into(),
            ..Default::default()
        }),
    )
    .await;
    let request = receive(stream).await;
    let Some(Payload::RpcRequest(rpc)) = request.payload else {
        panic!("expected identity request")
    };
    assert_eq!(rpc.method, "ReadIdentityBundle");
    send(
        stream,
        request.request_id,
        Payload::RpcResponse(proto::RpcResponse {
            result_json: json!({"node_endpoint_id":"aa".repeat(32)}).to_string(),
        }),
    )
    .await;
    if expect_publication {
        let request = receive(stream).await;
        assert!(
            matches!(&request.payload,Some(Payload::RpcRequest(rpc)) if rpc.method=="PublishPeerEvidence")
        );
        send(
            stream,
            request.request_id,
            Payload::RpcResponse(proto::RpcResponse {
                result_json: json!({"published":false}).to_string(),
            }),
        )
        .await;
    }
}

#[tokio::test]
async fn evidence_that_is_no_bond_reference_is_rejected_without_chain_calls() {
    let root = tempfile::Builder::new()
        .prefix("tempo-unreadable-proof-")
        .tempdir_in("/tmp")
        .unwrap();
    let (mut child, mut stream) = launch(env!("CARGO_BIN_EXE_tempo-fixture"), root.path()).await;
    initialize_config(&mut stream, root.path(), json!({}), true).await;
    send(&mut stream, 20, Payload::RpcRequest(proto::RpcRequest {
        method:"tools/call".into(),
        params_json:json!({"name":"verify_peer_evidence","arguments":{"mesh_id":"fixture","node_id":"bb".repeat(32),"evidence":{}}}).to_string(),
        invocation_id:None,
    })).await;
    let response = receive(&mut stream).await;
    let Some(Payload::RpcResponse(response)) = response.payload else {
        panic!("verification failed")
    };
    let value: Value = serde_json::from_str(&response.result_json).unwrap();
    assert_eq!(value["structuredContent"]["accepted"], false);
    assert!(value["structuredContent"]["reason"].is_string());
    assert!(value["structuredContent"]["valid_until"].as_u64().is_some());
    child.kill().await.unwrap();
}

#[tokio::test]
async fn prices_publications_and_requirements_are_independent_at_startup() {
    let contract = "0x1111111111111111111111111111111111111111";
    for (priced, published, required) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let root = tempfile::Builder::new()
            .prefix("tempo-policy-")
            .tempdir_in("/tmp")
            .unwrap();
        let (mut child, mut stream) =
            launch(env!("CARGO_BIN_EXE_tempo-fixture"), root.path()).await;
        let mut settings = json!({});
        if priced {
            settings["prices"] =
                json!({"model":{"input_per_million":"1","output_per_million":"1"}});
        }
        if published || required {
            settings["bonds"] = json!({"contract":contract});
        }
        // Without a stated requirement the standard bond applies.
        if required {
            settings["bonds"]["providers"] = json!({"minimum_amount":"1"});
        }
        initialize_config(&mut stream, root.path(), settings, true).await;
        if priced {
            let requirements = receive(&mut stream).await;
            assert!(
                matches!(requirements.payload, Some(Payload::ModelRequirementRequest(request)) if request.model=="model" && request.requires.len()==1)
            );
            send(
                &mut stream,
                requirements.request_id,
                Payload::ModelRequirementResponse(proto::ModelRequirementResponse {}),
            )
            .await;
        }
        if published {
            publish(&mut stream, 9, &"aa".repeat(32)).await;
        }
        let policy = control(&mut stream, 10, json!({"command":"policy"})).await;
        assert_eq!(policy["mode"], "free_only");
        let bonds = control(&mut stream, 11, json!({"command":"bond_context"})).await;
        assert_eq!(bonds["reference"].is_object(), published);
        let minimum = if required { "1" } else { "1000000" };
        assert_eq!(bonds["requirement"]["minimum_amount"], minimum);
        let pending = control(&mut stream, 12, json!({"command":"pending"})).await;
        assert!(pending["requests"].as_array().unwrap().is_empty());
        child.kill().await.unwrap();
    }
}

/// Publishes `bond_id` through the `bond_publish` control, answering the
/// plugin's publication request as the host.
async fn publish(stream: &mut LocalStream, id: u64, bond_id: &str) {
    let arguments = json!({"command":"bond_publish","bond_id":bond_id});
    send(
        stream,
        id,
        Payload::RpcRequest(proto::RpcRequest {
            method: "tools/call".into(),
            params_json: json!({"name":"control","arguments":arguments}).to_string(),
            invocation_id: None,
        }),
    )
    .await;
    let request = receive(stream).await;
    let Some(Payload::RpcRequest(rpc)) = &request.payload else {
        panic!("expected a publication request")
    };
    assert_eq!(rpc.method, "PublishPeerEvidence");
    let params: Value = serde_json::from_str(&rpc.params_json).unwrap();
    assert_eq!(params["evidence"]["bond_id"], bond_id);
    send(
        stream,
        request.request_id,
        Payload::RpcResponse(proto::RpcResponse {
            result_json: json!({"published":true}).to_string(),
        }),
    )
    .await;
    let response = receive(stream).await;
    assert_eq!(response.request_id, id);
    assert!(matches!(response.payload, Some(Payload::RpcResponse(_))));
}
/// The error text of one refused `control` command.
async fn control_error(stream: &mut LocalStream, id: u64, arguments: Value) -> String {
    send(
        stream,
        id,
        Payload::RpcRequest(proto::RpcRequest {
            method: "tools/call".into(),
            params_json: json!({"name":"control","arguments":arguments}).to_string(),
            invocation_id: None,
        }),
    )
    .await;
    let response = receive(stream).await;
    assert_eq!(response.request_id, id);
    match response.payload {
        Some(Payload::RpcResponse(response)) => response.result_json,
        Some(Payload::ErrorResponse(error)) => error.message,
        other => panic!("unexpected control answer: {other:?}"),
    }
}
/// The structured result of one successful `control` command.
async fn control(stream: &mut LocalStream, id: u64, arguments: Value) -> Value {
    send(
        stream,
        id,
        Payload::RpcRequest(proto::RpcRequest {
            method: "tools/call".into(),
            params_json: json!({"name":"control","arguments":arguments}).to_string(),
            invocation_id: None,
        }),
    )
    .await;
    let response = receive(stream).await;
    assert_eq!(response.request_id, id);
    let Some(Payload::RpcResponse(response)) = response.payload else {
        panic!("control failed")
    };
    let mut value: Value = serde_json::from_str(&response.result_json).unwrap();
    assert_ne!(value["isError"], true, "{value}");
    value["structuredContent"].take()
}

async fn expect_local_bond_context(stream: &mut LocalStream, expected_mesh: Option<&str>) {
    timeout(Duration::from_secs(3), async {
        loop {
            send(
                stream,
                12,
                Payload::RpcRequest(proto::RpcRequest {
                    method: "tools/call".into(),
                    params_json: json!({"name":"control","arguments":{"command":"bond_context"}})
                        .to_string(),
                    invocation_id: None,
                }),
            )
            .await;
            let response = receive(stream).await;
            let Some(Payload::RpcResponse(response)) = response.payload else {
                panic!("bond context must not perform a host lookup");
            };
            let value: Value = serde_json::from_str(&response.result_json).unwrap();
            let context = &value["structuredContent"];
            assert_eq!(context["node_id"], "aa".repeat(32));
            assert_eq!(context["namespace"], "tempo.bond.v1");
            if context["mesh_id"].as_str() == expected_mesh {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bond_context_tracks_snapshots_and_mesh_changes_without_host_lookups() {
    let root = tempfile::Builder::new()
        .prefix("tempo-mesh-context-")
        .tempdir_in("/tmp")
        .unwrap();
    let (mut child, mut stream) = launch(env!("CARGO_BIN_EXE_tempo-fixture"), root.path()).await;
    initialize(&mut stream, root.path()).await;
    expect_local_bond_context(&mut stream, Some("fixture")).await;
    send(&mut stream, 40, Payload::RpcRequest(proto::RpcRequest {
        method: "tools/call".into(),
        params_json: json!({"name":"control","arguments":{"command":"bond_lock","operation_id":uuid::Uuid::new_v4().to_string(),"mesh_id":"unintended-mesh","amount":"1","exit_delay_seconds":86400,"salt":"00".repeat(32)}}).to_string(),
        invocation_id: None,
    })).await;
    let response = receive(&mut stream).await;
    assert!(format!("{response:?}").contains("intended mesh differs"));
    for mesh in ["changed-mesh", ""] {
        send(
            &mut stream,
            0,
            Payload::MeshEvent(proto::MeshEvent {
                kind: proto::mesh_event::Kind::MeshIdUpdated as i32,
                local_peer_id: "aa".repeat(32),
                mesh_id: mesh.into(),
                ..Default::default()
            }),
        )
        .await;
        expect_local_bond_context(&mut stream, (!mesh.is_empty()).then_some(mesh)).await;
    }
    child.kill().await.unwrap();
}

#[tokio::test]
async fn combined_plugin_starts_inspects_and_restarts_without_a_wallet_provider() {
    let root = tempfile::Builder::new()
        .prefix("pay-ipc-")
        .tempdir_in("/tmp")
        .unwrap();
    for _ in 0..2 {
        let (mut child, mut stream) =
            launch(env!("CARGO_BIN_EXE_tempo-fixture"), root.path()).await;
        initialize(&mut stream, root.path()).await;
        let fund = control(&mut stream, 2, json!({"command":"fund"})).await;
        assert_eq!(fund["address"], format!("0x{}", "11".repeat(20)));
        assert_eq!(fund["network"], "testnet");
        let balance = control(&mut stream, 3, json!({"command":"balance"})).await;
        assert_eq!(balance["balance"], "1000000000");
        let pending = control(&mut stream, 4, json!({"command":"pending"})).await;
        assert_eq!(pending["requests"], json!([]));
        assert_eq!(pending["attempts"], json!([]));
        let history = json!({"command":"transactions","limit":10});
        let transactions = control(&mut stream, 6, history).await;
        assert_eq!(transactions["channels"], json!([]));
        assert_eq!(transactions["attempts"], json!([]));
        send(
            &mut stream,
            5,
            Payload::ShutdownRequest(proto::ShutdownRequest {
                reason: "test complete".into(),
            }),
        )
        .await;
        assert!(matches!(
            receive(&mut stream).await.payload,
            Some(Payload::ShutdownResponse(_))
        ));
        assert!(
            timeout(Duration::from_secs(10), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    assert!(!root.path().join("wallet/wallet.json").exists());
}

#[tokio::test]
async fn production_entry_point_cannot_select_the_fake_wallet_from_environment() {
    let root = tempfile::Builder::new()
        .prefix("pay-native-")
        .tempdir_in("/tmp")
        .unwrap();
    // Without a wallet the node starts to verify bonds, and refuses payments.
    let (mut child, mut stream) = launch(env!("CARGO_BIN_EXE_tempo"), root.path()).await;
    initialize_config(&mut stream, root.path(), json!({}), true).await;
    // `fund` would create a wallet and call the faucet, so the test reads
    // the balance, which needs a wallet without creating one.
    let balance = control_error(&mut stream, 10, json!({"command":"balance"})).await;
    assert!(balance.contains("no wallet yet"), "{balance}");
    assert!(!root.path().join("payments/wallet.json").exists());
    let automatic = json!({"command":"policy","value":{"mode":"automatic","daily_budget":"10"}});
    let policy = control_error(&mut stream, 11, automatic).await;
    assert!(policy.contains("need a wallet"), "{policy}");
    child.kill().await.unwrap();
    // A priced model needs a wallet, so the node does not start.
    let (mut child, mut stream) = launch(env!("CARGO_BIN_EXE_tempo"), root.path()).await;
    let priced = json!({"prices":{"model":{"input_per_million":"1","output_per_million":"1"}}});
    initialize_config(&mut stream, root.path(), priced, false).await;
    assert!(
        !timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!root.path().join("chain.sqlite").exists());
    assert!(!root.path().join("wallet/wallet.json").exists());
}

#[tokio::test]
async fn tempo_requires_the_evidence_capability_before_opening_a_wallet() {
    let root = tempfile::Builder::new()
        .prefix("tempo-old-host-")
        .tempdir_in("/tmp")
        .unwrap();
    let (_child, mut stream) = launch(env!("CARGO_BIN_EXE_tempo-fixture"), root.path()).await;
    let config = root.path().join("payments.json");
    let settings = json!({
        "data_dir": root.path().join("payments"),
        "bonds": {
            "contract": "0x1111111111111111111111111111111111111111",
            "providers": {"minimum_amount": "1"}
        }
    });
    std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
    let host_capabilities = vec![mesh_llm_plugin::host_capabilities::OPENAI_EXCHANGE.into()];
    send(
        &mut stream,
        1,
        Payload::InitializeRequest(proto::InitializeRequest {
            host_protocol_version: PROTOCOL_VERSION,
            host_version: "old-host".into(),
            host_info_json: "{}".into(),
            mesh_visibility: proto::MeshVisibility::Private.into(),
            host_capabilities,
            settings_json: json!({"config_path":config}).to_string(),
        }),
    )
    .await;
    let response = receive(&mut stream).await;
    assert!(
        matches!(&response.payload, Some(Payload::ErrorResponse(error)) if error.message.contains(mesh_llm_plugin::peer_evidence::HOST_CAPABILITY)),
        "{response:?}"
    );
    assert!(!root.path().join("chain.sqlite").exists());
    assert!(!root.path().join("payments").exists());
}
