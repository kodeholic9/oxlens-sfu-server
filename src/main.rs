// author: kodeholic (powered by Claude)

#[tokio::main]
async fn main() {
    if let Err(e) = light_livechat::run_server().await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
