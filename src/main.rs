// author: kodeholic (powered by Claude)

#[tokio::main]
async fn main() {
    if let Err(e) = oxlens_sfu_server::run_server().await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
