use localhost::{config::Config, server};
use std::path::PathBuf;

fn main() {
    if let Err(message) = run() {
        eprintln!("LocalHost: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut config_path = PathBuf::from("localhost.conf");
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--config" | "-c" => {
                config_path = args
                    .next()
                    .ok_or_else(|| "--config requires a file path".to_string())?
                    .into();
            }
            "--help" | "-h" => {
                println!("Usage: localhost [--config PATH]\nDefault config: localhost.conf");
                return Ok(());
            }
            other => return Err(format!("unknown argument '{other}' (use --help)")),
        }
    }
    let config = Config::load(&config_path)?;
    server::run(config).map_err(|error| format!("server stopped: {error}"))
}
