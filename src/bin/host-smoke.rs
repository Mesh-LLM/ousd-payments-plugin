//! Two real Mesh hosts, canned inference, and either offline wallets or faucet-funded Moderato wallets.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[path = "host-smoke/bonds.rs"]
mod bonds;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    mesh_bin: PathBuf,
    /// Fresh owner-only output directory; contains private invitation/config/log files.
    #[arg(long)]
    output_dir: PathBuf,
    /// Create fresh wallets, request faucet coins, and settle real Moderato testnet transactions.
    #[arg(long)]
    tempo_testnet: bool,
    /// Also deploy and test indefinite bonds and delayed exits on Moderato.
    #[arg(long, requires = "tempo_testnet")]
    tempo_bonds: bool,
    /// Resume the bond stages using the existing wallets, escrow and journals.
    #[arg(long, requires = "tempo_bonds")]
    resume_bonds: bool,
}
const TEMPO_RPC: &str = "https://rpc.moderato.tempo.xyz";
const TEST_OUSD: &str = "0x20c0000000000000000000006a37da5c996874be";

async fn tempo_rpc(client: &reqwest::Client, method: &str, params: Value) -> Result<Value> {
    let response: Value = client
        .post(TEMPO_RPC)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        response.get("error").is_none(),
        "{method} failed: {}",
        response["error"]
    );
    response
        .get("result")
        .cloned()
        .context("missing testnet RPC result")
}

async fn prepare_testnet_wallet(
    client: &reqwest::Client,
    bins: &Path,
    directory: &Path,
) -> Result<()> {
    ensure!(
        tempo_rpc(client, "eth_chainId", json!([])).await? == "0xa5bf",
        "faucet trial requires Moderato chain 42431"
    );
    private_write(
        &directory.join("password"),
        &hex::encode(rand::random::<[u8; 32]>()),
    )?;
    let output = Command::new(bins.join("tempo"))
        .arg("--config")
        .arg(directory.join("payments.json"))
        .args(["wallet", "init"])
        .output()?;
    ensure!(
        output.status.success(),
        "explicit testnet wallet initialization failed"
    );
    let profile: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        profile["chain_id"] == 42431 && profile["token"] == TEST_OUSD,
        "wallet is not test OUSD on Moderato"
    );
    let address = profile["address"]
        .as_str()
        .context("wallet address missing")?;
    let funded = tempo_rpc(client, "tempo_fundAddress", json!([address])).await?;
    private_write(
        &directory.join("faucet.json"),
        &serde_json::to_string_pretty(&funded)?,
    )?;
    let data = format!("0x70a08231{:0>64}", address.trim_start_matches("0x"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let value = tempo_rpc(
            client,
            "eth_call",
            json!([{"to":TEST_OUSD,"data":data},"finalized"]),
        )
        .await?;
        let balance = u64::from_str_radix(
            value
                .as_str()
                .context("invalid test OUSD balance")?
                .trim_start_matches("0x"),
            16,
        )?;
        if balance >= 10_000 {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "faucet test OUSD did not finalize; wallet state preserved"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!(
        "{} wallet faucet-funded on Moderato: {address}",
        directory.file_name().unwrap().to_string_lossy()
    );
    Ok(())
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Clone, Copy)]
struct Ports {
    api: u16,
    console: u16,
}
impl Ports {
    fn new() -> Result<Self> {
        Ok(Self {
            api: port()?,
            console: port()?,
        })
    }
}
fn trial_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(45))
        .build()?)
}
fn port() -> Result<u16> {
    Ok(
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port(),
    )
}
fn private_write(path: &Path, content: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
fn quote(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}
fn package_plugins(directory: &Path, bins: &Path, testnet: bool) -> Result<Vec<(String, PathBuf)>> {
    let packages = directory.join("packages");
    std::fs::create_dir(&packages)?;
    let mut archives = Vec::new();
    for name in ["tempo", "demo-backend-fixture"] {
        let stage = packages.join(name);
        std::fs::create_dir(&stage)?;
        std::fs::copy(
            bins.join(if name == "tempo" && !testnet {
                "tempo-fixture"
            } else {
                name
            }),
            stage.join(name),
        )?;
        private_write(
            &stage.join("plugin.toml"),
            &format!(
                "name = \"{name}\"\nversion = \"{}\"\n",
                env!("CARGO_PKG_VERSION")
            ),
        )?;
        let output = Command::new(stage.join(name)).arg("--manifest").output()?;
        ensure!(
            output.status.success(),
            "manifest generation failed for {name}"
        );
        let manifest = String::from_utf8(output.stdout)?;
        serde_json::from_str::<Value>(&manifest)?;
        private_write(&stage.join("plugin-manifest.json"), &manifest)?;
        let archive = packages.join(format!("{name}.tar.gz"));
        let output = Command::new("tar")
            .arg("-C")
            .arg(&packages)
            .arg("-czf")
            .arg(&archive)
            .arg(name)
            .output()?;
        ensure!(
            output.status.success(),
            "archive creation failed for {name}"
        );
        archives.push((name.to_owned(), archive));
    }
    Ok(archives)
}
fn install_plugins(
    mesh: &Path,
    directory: &Path,
    archives: &[(String, PathBuf)],
    include_backend: bool,
) -> Result<()> {
    std::fs::create_dir(directory)?;
    let bootstrap = directory.join("install-config.toml");
    private_write(&bootstrap, "version = 1\n")?;
    for (name, archive) in archives {
        if name == "demo-backend-fixture" && !include_backend {
            continue;
        }
        let output = Command::new(mesh)
            .args(["plugins", "install", "--archive"])
            .arg(archive)
            .args(["--name", name, "--version", env!("CARGO_PKG_VERSION")])
            .env("MESH_LLM_CONFIG", &bootstrap)
            .env("MESH_LLM_PLUGIN_DIR", directory.join("installed-plugins"))
            .env("MESH_LLM_NODE_KEY_PATH", directory.join("node.key"))
            .env("MESH_LLM_RUNTIME_ROOT", directory.join("runtime"))
            .env("MESH_LLM_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()?;
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        private_write(&directory.join(format!("install-{name}.log")), &log)?;
        ensure!(
            output.status.success(),
            "install {name} failed; inspect the private install log"
        );
    }
    Ok(())
}
fn config(directory: &Path, backend: Option<u16>, seller: &str, testnet: bool) -> Result<()> {
    let seller_mode = backend.is_some();
    let price = json!({"mesh-payment-fixture":{"input_per_million":"1000000","output_per_million":"1000000"}});
    let mut payments = json!({"data_dir":directory.join("payments"),"wallet":{},"admission_deposit":"10",
        "prices":if seller_mode {price.clone()} else {json!({})},
        "policy":if seller_mode {json!({"mode":"free_only"})} else {json!({"mode":"automatic","daily_budget":"20000"})},
        "buy_peers":if seller_mode {vec![]} else {vec![seller]},"buy_prices":price,
        // The cap covers the trial request's worst case: its body bytes as
        // input tokens, its 4 output tokens and the upfront.
        "cap":"200","channel_topup":"200"});
    if testnet {
        payments["wallet"] = json!({"network":"testnet","rpc_url":TEMPO_RPC,
            "password_file":directory.join("password"),"max_payment":"200","max_fee":"10000"});
    }
    let mut config = format!(
        r#"version = 1

[[plugin]]
name = "tempo"
[plugin.settings]
config_path = {}

[plugin.openai_exchange_grant]
endpoints = ["chat_completions", "completions", "responses"]
phases = ["request_received", "backend_selected", "exchange_finished"]
request_body = true
effective_request_body = true
admission = true
read_identity_bundle = true
deadline_ms = 30000
max_body_bytes = 65536
max_queue_bytes = 2097152
max_in_flight = 32
failure_policy = "required"
"#,
        quote(&directory.join("payments.json"))
    );
    if let Some(port) = backend {
        config.push_str(&format!(
            "\n[[plugin]]\nname = \"demo-backend-fixture\"\nurl = \"http://127.0.0.1:{port}/v1\"\n"
        ));
    } else {
        config.push_str("\n[runtime]\nmode = \"client\"\n");
    }
    private_write(&directory.join("config.toml"), &config)?;
    save_settings(directory, &payments)
}
fn save_settings(directory: &Path, settings: &Value) -> Result<()> {
    let settings_path = directory.join("payments.json");
    if settings_path.exists() {
        std::fs::write(settings_path, serde_json::to_vec_pretty(settings)?)?;
    } else {
        private_write(&settings_path, &serde_json::to_string_pretty(settings)?)?;
    }
    Ok(())
}
fn start(
    mesh: &Path,
    directory: &Path,
    ports: Ports,
    invitation: Option<&Path>,
    testnet: bool,
) -> Result<Running> {
    let log_path = directory.join("host.log");
    if !log_path.exists() {
        private_write(&log_path, "")?;
    }
    let log = OpenOptions::new().append(true).open(log_path)?;
    let mut command = Command::new(mesh);
    if !testnet {
        // The offline fixture has no chain to bond on, so it runs without
        // a verifier and its payment smoke reaches the unbonded seller.
        command.env("TEMPO_FIXTURE_NO_VERIFIER", "1");
    }
    command
        .arg(if invitation.is_some() {
            "client"
        } else {
            "serve"
        })
        .args([
            "--log-format",
            "json",
            "--disable-iroh-relays",
            "--bind-ip",
            "127.0.0.1",
            "--mesh-discovery-mode",
            "mdns",
        ])
        .arg("--port")
        .arg(ports.api.to_string())
        .arg("--console")
        .arg(ports.console.to_string())
        .env("MESH_LLM_CONFIG", directory.join("config.toml"))
        .env("MESH_LLM_NO_DEFAULT_PLUGINS", "1")
        .env("MESH_LLM_DATA_DIR", directory.join("host-data"))
        .env("MESH_LLM_PLUGIN_DIR", directory.join("installed-plugins"))
        .env("MESH_LLM_RUNTIME_ROOT", directory.join("runtime"))
        .env("MESH_LLM_NODE_KEY_PATH", directory.join("node.key"))
        .env("MESH_LLM_NO_UPDATE_CHECK", "1")
        .env("MESH_LLM_NO_RUNTIME_DOWNLOAD", "1")
        .env_remove("MESH_LLM_JOIN")
        .env_remove("MESH_LLM_JOIN_FILE")
        .env_remove("MESH_LLM_EPHEMERAL_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if !testnet {
        command
            .env(
                "MESH_LLM_OFFLINE_CHAIN_PATH",
                directory
                    .parent()
                    .context("missing smoke root")?
                    .join("OFFLINE-fixture.sqlite3"),
            )
            .env(
                "MESH_LLM_OFFLINE_ADDRESS",
                format!(
                    "0x{}",
                    (if invitation.is_some() { "11" } else { "22" }).repeat(20)
                ),
            );
    }
    if let Some(path) = invitation {
        command
            .args(["--max-vram", "0", "--no-enumerate-host"])
            .arg("--join-file")
            .arg(path);
    }
    Ok(Running(command.spawn().context("start Mesh smoke node")?))
}
async fn get(client: &reqwest::Client, port: u16, path: &str) -> Result<Value> {
    Ok(client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
async fn control(client: &reqwest::Client, port: u16, input: Value) -> Result<Value> {
    plugin_operation(client, port, "control", input).await
}
async fn plugin_operation(
    client: &reqwest::Client,
    port: u16,
    operation: &str,
    input: Value,
) -> Result<Value> {
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/api/plugins/tempo/tools/{operation}"
        ))
        .json(&input)
        .send()
        .await?;
    let code = response.status();
    let response: Value = response.json().await?;
    ensure!(code.is_success(), "{operation} HTTP {code}: {response}");
    ensure!(
        response["isError"] != true,
        "{operation} returned an error: {response}"
    );
    if let Some(data) = response
        .get("structuredContent")
        .or_else(|| response.get("structured_content"))
    {
        return Ok(data.clone());
    }
    if let Some(text) = response["content"]
        .as_array()
        .and_then(|xs| xs.first())
        .and_then(|x| x["text"].as_str())
    {
        return Ok(serde_json::from_str(text)?);
    }
    Ok(response)
}
async fn status(client: &reqwest::Client, port: u16) -> Result<Value> {
    control(client, port, json!({"command":"bond_context"})).await
}
async fn wait<F, Fut>(label: &str, child: &mut Running, mut action: F) -> Result<Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(exit) = child.0.try_wait()? {
            anyhow::bail!(
                "Mesh exited ({exit}) while waiting for {label}; inspect private host logs"
            );
        }
        if let Ok(value) = action().await {
            return Ok(value);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for {label}; inspect private host logs"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
async fn cleared(client: &reqwest::Client, port: u16, child: &mut Running) -> Result<Value> {
    wait("cleared payment debt", child, || async {
        let value = control(client, port, json!({"command":"pending"})).await?;
        ensure!(
            value["requests"].as_array().is_some_and(Vec::is_empty),
            "settlement pending"
        );
        Ok(value)
    })
    .await
}
async fn buyer_ready(
    client: &reqwest::Client,
    ports: Ports,
    child: &mut Running,
) -> Result<String> {
    let state = wait("buyer payment plugin", child, || async {
        let state = status(client, ports.console).await?;
        ensure!(
            control(client, ports.console, json!({"command":"policy"})).await?["mode"]
                == "automatic",
            "buyer plugin not ready"
        );
        ensure!(
            state["node_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
            "buyer full identity not ready"
        );
        Ok(state)
    })
    .await?;
    wait("remote fixture model", child, || async {
        let value = get(client, ports.api, "/v1/models").await?;
        ensure!(
            value["data"]
                .as_array()
                .is_some_and(|xs| xs.iter().any(|x| x["id"] == "mesh-payment-fixture")),
            "model not advertised"
        );
        Ok(value)
    })
    .await?;
    Ok(state["node_id"]
        .as_str()
        .context("missing buyer identity")?
        .to_owned())
}
async fn start_seller(
    mesh: &Path,
    root: &Path,
    ports: Ports,
    client: &reqwest::Client,
    testnet: bool,
) -> Result<(Running, Value)> {
    let mut child = start(mesh, &root.join("seller"), ports, None, testnet)?;
    let state = wait("seller payment plugin", &mut child, || async {
        let state = status(client, ports.console).await?;
        ensure!(
            state["node_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
            "seller full identity not ready"
        );
        Ok(state)
    })
    .await?;
    let host = get(client, ports.console, "/api/status").await?;
    let invitation = host["token"].as_str().context("missing invitation")?;
    let path = root.join("invite.token");
    if path.exists() {
        std::fs::write(path, invitation)?;
    } else {
        private_write(&path, invitation)?;
    }
    Ok((child, state))
}
async fn start_buyer(
    mesh: &Path,
    root: &Path,
    ports: Ports,
    client: &reqwest::Client,
    testnet: bool,
) -> Result<Running> {
    let mut child = start(
        mesh,
        &root.join("buyer"),
        ports,
        Some(&root.join("invite.token")),
        testnet,
    )?;
    wait("buyer payment plugin", &mut child, || {
        status(client, ports.console)
    })
    .await?;
    Ok(child)
}
async fn execute(args: Args) -> Result<()> {
    let mesh = args.mesh_bin.canonicalize()?;
    if args.resume_bonds {
        return bonds::resume(&mesh, &args.output_dir).await;
    }
    ensure!(!args.output_dir.exists(), "--output-dir must be fresh");
    std::fs::create_dir_all(&args.output_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&args.output_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let directory = args.output_dir.canonicalize()?;
    let bins = std::env::current_exe()?
        .parent()
        .context("binary directory missing")?
        .to_path_buf();
    for binary in [
        if args.tempo_testnet {
            "tempo"
        } else {
            "tempo-fixture"
        },
        "demo-backend-fixture",
    ] {
        ensure!(
            bins.join(binary).is_file(),
            "build missing companion binary {binary}"
        );
    }
    let chain = directory.join("OFFLINE-fixture.sqlite3");
    let seller_dir = directory.join("seller");
    let buyer_dir = directory.join("buyer");
    let seller_ports = Ports::new()?;
    let buyer_ports = Ports::new()?;
    let archives = package_plugins(&directory, &bins, args.tempo_testnet)?;
    install_plugins(&mesh, &seller_dir, &archives, true)?;
    install_plugins(&mesh, &buyer_dir, &archives, false)?;
    config(&seller_dir, Some(port()?), "", args.tempo_testnet)?;
    let client = trial_client()?;
    if args.tempo_testnet {
        prepare_testnet_wallet(&client, &bins, &seller_dir).await?;
    }
    let (mut seller, payment_status) =
        start_seller(&mesh, &directory, seller_ports, &client, args.tempo_testnet).await?;
    let seller_id = payment_status["node_id"]
        .as_str()
        .context("payment plugin status missing full node identity")?;
    let public_wallet = client
        .post(format!(
            "http://127.0.0.1:{}/api/plugins/tempo/tools/wallet_pay",
            seller_ports.console
        ))
        .json(&json!({}))
        .send()
        .await?;
    ensure!(
        public_wallet.status().as_u16() == 404,
        "wallet spending operation was exposed as a public tool"
    );
    config(&buyer_dir, None, seller_id, args.tempo_testnet)?;
    if args.tempo_testnet {
        prepare_testnet_wallet(&client, &bins, &buyer_dir).await?;
    }
    // A buyer requiring a bond never selects, let alone pays, an unbonded
    // seller, so the bond trial starts the buyer itself.
    if args.tempo_bonds {
        return bonds::Trial {
            mesh: &mesh,
            root: &directory,
            client: &client,
            seller_ports,
            buyer_ports,
            seller: Some(seller),
            buyer: None,
        }
        .execute()
        .await;
    }
    let mut buyer =
        start_buyer(&mesh, &directory, buyer_ports, &client, args.tempo_testnet).await?;
    let buyer_id = buyer_ready(&client, buyer_ports, &mut buyer).await?;
    let before = if args.tempo_testnet {
        json!({"buyer":control(&client,buyer_ports.console,json!({"command":"balance"})).await?,
            "seller":control(&client,seller_ports.console,json!({"command":"balance"})).await?})
    } else {
        Value::Null
    };
    for number in 0..2 {
        let streaming = number == 1;
        let mut request = json!({"model":"mesh-payment-fixture","messages":[{"role":"user","content":"test"}],"max_tokens":4,"stream":streaming});
        if streaming {
            request["stream_options"] = json!({"include_usage":true});
        }
        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                buyer_ports.api
            ))
            .json(&request)
            .send()
            .await?;
        let code = response.status();
        let text = response.text().await?;
        if !code.is_success() {
            private_write(&directory.join("inference-error.json"), &text)?;
        }
        ensure!(
            code.is_success(),
            "fixture inference rejected (HTTP {code}); inspect private logs"
        );
        let usage = if streaming {
            ensure!(
                text.contains("[DONE]") && text.contains("OFFLINE FIXTURE RESPONSE"),
                "incomplete fixture SSE stream"
            );
            text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find_map(|chunk| chunk.get("usage").filter(|u| u.is_object()).cloned())
                .context("stream omitted terminal usage")?
        } else {
            let body: Value = serde_json::from_str(&text)?;
            body["usage"].clone()
        };
        ensure!(
            usage["prompt_tokens"] == 1 && usage["completion_tokens"] == 4,
            "wrong fixture usage"
        );
        cleared(&client, buyer_ports.console, &mut buyer).await?;
        cleared(&client, seller_ports.console, &mut seller).await?;
        if number == 0 {
            drop(buyer);
            buyer =
                start_buyer(&mesh, &directory, buyer_ports, &client, args.tempo_testnet).await?;
            let restarted_id = buyer_ready(&client, buyer_ports, &mut buyer).await?;
            ensure!(
                restarted_id == buyer_id,
                "buyer payment identity changed across restart"
            );
        }
    }
    if args.tempo_testnet {
        testnet_result(&client, buyer_ports, seller_ports, before, &directory).await
    } else {
        let connection = rusqlite::Connection::open(&chain)?;
        mpp_result(&connection, &directory)
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    execute(Args::parse()).await
}

fn mpp_result(connection: &rusqlite::Connection, directory: &Path) -> Result<()> {
    let mut query =
        connection.prepare("SELECT deposit,closure FROM fixture_channels ORDER BY id")?;
    let rows = query
        .query_map([], |r| {
            Ok((r.get::<_, u64>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        rows == [(200, None)],
        "expected one open channel reused by both inference requests"
    );
    private_write(
        &directory.join("result.json"),
        &serde_json::to_string_pretty(&json!({
            "offline":true,"real_mesh_hosts":2,"inference_requests":2,"buffered_requests":1,"streaming_requests":1,
            "fabricated_channels":1,"channel_deposit_atomic":200,"upfront_atomic":10,"cost_per_request_atomic":5,
            "buyer_restart_identity_preserved":true,"wallet_spending_not_http_exposed":true,"network_transactions_sent":0
        }))?,
    )?;
    Ok(())
}

async fn testnet_result(
    client: &reqwest::Client,
    buyer: Ports,
    seller: Ports,
    before: Value,
    directory: &Path,
) -> Result<()> {
    let buyer_history = control(
        client,
        buyer.console,
        json!({"command":"transactions","limit":20}),
    )
    .await?;
    let seller_history = control(
        client,
        seller.console,
        json!({"command":"transactions","limit":20}),
    )
    .await?;
    let rows = buyer_history["requests"]
        .as_array()
        .context("missing buyer history")?;
    let seller_rows = seller_history["requests"]
        .as_array()
        .context("missing seller history")?;
    ensure!(
        rows.len() == 2 && seller_rows.len() == 2,
        "expected two paid native requests per peer"
    );
    for row in rows {
        ensure!(row["bill"]["cost"] == "5", "wrong native bill");
        ensure!(
            seller_rows.iter().any(|seller| seller["id"] == row["id"]
                && seller["bill"] == row["bill"]
                && seller["channel"] == row["channel"]),
            "buyer and seller disagree on the bill"
        );
    }
    let pending = control(client, buyer.console, json!({"command":"pending"})).await?;
    let channels = pending["channels"].clone();
    let channel = &channels[0];
    ensure!(
        channels.as_array().is_some_and(|c| c.len() == 1)
            && channel["running_total"] == "10"
            && channel["voucher"]["cum"] == "10",
        "expected one channel with both bills paid"
    );
    let receipt = tempo_rpc(
        client,
        "eth_getTransactionReceipt",
        json!([channel["funding"]["open_tx"]]),
    )
    .await?;
    ensure!(
        receipt["status"] == "0x1",
        "native transaction did not succeed"
    );
    let after = json!({"buyer":control(client,buyer.console,json!({"command":"balance"})).await?,
        "seller":control(client,seller.console,json!({"command":"balance"})).await?});
    let result = json!({"offline":false,"chain_id":42431,"asset":TEST_OUSD,"real_mesh_hosts":2,
        "canned_inference":true,"inference_requests":2,"buffered_requests":1,"streaming_requests":1,
        "buyer_restart_identity_preserved":true,"wallet_spending_not_http_exposed":true,
        "channel_deposit_atomic":100,"upfront_atomic":10,"cost_per_request_atomic":5,
        "before":before,"after":after,"channel":channel,"open_receipt":receipt,"requests":rows});
    private_write(
        &directory.join("result.json"),
        &serde_json::to_string_pretty(&result)?,
    )?;
    println!(
        "Two Moderato requests were paid from one channel: an upfront of 10 test OUSD atomic units covered both bills of 5."
    );
    Ok(())
}
