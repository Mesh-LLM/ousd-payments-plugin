#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mesh_llm_tempo::cli::run().await
}
