#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = usage::cli::run().await;
    std::process::ExitCode::from(code as u8)
}
