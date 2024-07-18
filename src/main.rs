use chrono::Local;
use reqwest::Error;
use std::env::{self};
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("httping: missing URL");
        eprintln!("Usage: httping [URL]");
        std::process::exit(1);
    }

    if args.len() > 2 {
        eprintln!("httping: can only use one URL");
        eprintln!("Usage: httping [URL]");
        std::process::exit(1);
    }

    let url = &args[1];

    if Url::parse(url).is_err() {
        eprintln!("Invalid URL: {}", url);
        std::process::exit(1);
    }

    let now = Local::now();
    let response = reqwest::get(url).await?;
    let status_code = response.status();

    println!("{} {} {}", now.to_rfc3339(), status_code, url);

    Ok(())
}
