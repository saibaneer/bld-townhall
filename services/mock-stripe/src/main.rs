#![forbid(unsafe_code)]

use mock_stripe::MockStripe;
use std::{io::Write as _, process::ExitCode};

fn parse_port() -> Result<u16, String> {
    let mut args = std::env::args().skip(1);
    let mut port = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--port" => {
                port = Some(
                    args.next()
                        .ok_or("--port needs a value")?
                        .parse()
                        .map_err(|_| "--port needs a number")?,
                );
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    port.ok_or_else(|| "--port is required".to_owned())
}

#[tokio::main]
async fn main() -> ExitCode {
    let port = match parse_port() {
        Ok(port) => port,
        Err(problem) => {
            eprintln!("mock-stripe: {problem}");
            eprintln!("usage: mock-stripe --port <n>");
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mock-stripe: cannot bind port {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let bound_port = listener.local_addr().map_or(port, |address| address.port());
    println!("READY {bound_port}");
    let _ = std::io::stdout().flush();

    if axum::serve(listener, MockStripe::new().router())
        .await
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
