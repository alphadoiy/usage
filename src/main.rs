#[tokio::main]
async fn main() {
    let code = usage::cli::run().await;
    std::process::exit(code);
}
