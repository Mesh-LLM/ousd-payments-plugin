#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--manifest") {
        println!(
            "{}",
            mesh_llm_plugin::package_manifest_json(&mesh_llm_tempo::runtime::fixture_manifest())?
        );
        return Ok(());
    }
    mesh_llm_tempo::runtime::run_fixture().await
}
