//! Command-line parsing and runtime behavior for `HTTPing`.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::{Local, SecondsFormat};
use clap::{Arg, ArgMatches, Command};
use reqwest::{Client, Url};
use tokio::net::lookup_host;
use tokio::time::{Instant as TokioInstant, sleep_until, timeout};

/// Ping an HTTP endpoint and report response latency.
#[derive(Debug)]
pub struct Cli {
    /// Number of probes to send. By default, probes continue until Ctrl-C.
    pub count: Option<u64>,

    /// Minimum time between probe starts.
    pub interval: Duration,

    /// Maximum time for DNS setup and for each probe.
    pub timeout: Duration,

    /// HTTP or HTTPS URL to probe.
    pub url: Url,
}

impl Cli {
    /// Parse the process command line, printing an error and exiting on invalid input.
    #[must_use]
    pub fn parse() -> Self {
        Self::from_matches(&command().get_matches())
    }

    #[cfg(test)]
    fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        command()
            .try_get_matches_from(arguments)
            .map(|matches| Self::from_matches(&matches))
    }

    fn from_matches(matches: &ArgMatches) -> Self {
        Self {
            count: matches.get_one::<u64>("count").copied(),
            interval: *matches
                .get_one::<Duration>("interval")
                .expect("interval has a default"),
            timeout: *matches
                .get_one::<Duration>("timeout")
                .expect("timeout has a default"),
            url: matches
                .get_one::<Url>("url")
                .expect("URL is required")
                .clone(),
        }
    }
}

fn command() -> Command {
    Command::new("httping")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Ping an HTTP endpoint and report response latency")
        .arg(
            Arg::new("count")
                .short('c')
                .long("count")
                .value_name("COUNT")
                .help("Number of probes to send. By default, probes continue until Ctrl-C")
                .value_parser(clap::value_parser!(u64).range(1..)),
        )
        .arg(
            Arg::new("interval")
                .short('i')
                .long("interval")
                .value_name("DURATION")
                .help("Minimum time between probe starts")
                .default_value("1s")
                .value_parser(parse_duration),
        )
        .arg(
            Arg::new("timeout")
                .short('t')
                .long("timeout")
                .value_name("DURATION")
                .help("Maximum time for DNS setup and for each probe")
                .default_value("10s")
                .value_parser(parse_duration),
        )
        .arg(
            Arg::new("url")
                .value_name("URL")
                .help("HTTP or HTTPS URL to probe")
                .required(true)
                .value_parser(parse_url),
        )
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }
    Ok(duration)
}

fn parse_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL scheme must be http or https".to_owned());
    }
    Ok(url)
}

#[derive(Default)]
struct Statistics {
    attempts: u64,
    responses: u64,
    total: Duration,
    minimum: Option<Duration>,
    maximum: Option<Duration>,
    failed: bool,
}

impl Statistics {
    fn response(&mut self, elapsed: Duration) {
        self.attempts += 1;
        self.responses += 1;
        self.total += elapsed;
        self.minimum = Some(self.minimum.map_or(elapsed, |value| value.min(elapsed)));
        self.maximum = Some(self.maximum.map_or(elapsed, |value| value.max(elapsed)));
    }

    fn error(&mut self) {
        self.attempts += 1;
        self.failed = true;
    }

    fn summary(&self) -> String {
        let lost_attempts = self.attempts - self.responses;
        let loss_tenths = if self.attempts == 0 {
            0
        } else {
            (u128::from(lost_attempts) * 1_000) / u128::from(self.attempts)
        };
        let mut output = format!(
            "\n--- httping statistics ---\nattempts: {}, responses: {}, loss: {}.{}%\n",
            self.attempts,
            self.responses,
            loss_tenths / 10,
            loss_tenths % 10
        );
        if let (Some(minimum), Some(maximum)) = (self.minimum, self.maximum) {
            let average = self.total / u32::try_from(self.responses).unwrap_or(u32::MAX);
            let _ = writeln!(
                output,
                "time: min {}, avg {}, max {}",
                humantime::format_duration(minimum),
                humantime::format_duration(average),
                humantime::format_duration(maximum)
            );
        } else {
            output.push_str("time: min n/a, avg n/a, max n/a\n");
        }
        output
    }
}

/// Run `HTTPing` and return its process exit status.
pub async fn run(cli: Cli) -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_with_writers(cli, &mut stdout.lock(), &mut stderr.lock()).await
}

#[allow(clippy::too_many_lines)]
async fn run_with_writers(cli: Cli, output: &mut impl Write, errors: &mut impl Write) -> ExitCode {
    let Some(host) = cli.url.host_str() else {
        let _ = writeln!(errors, "httping: URL has no hostname");
        return ExitCode::FAILURE;
    };
    let Some(port) = cli.url.port_or_known_default() else {
        let _ = writeln!(errors, "httping: URL has no effective port");
        return ExitCode::FAILURE;
    };

    let addresses = match timeout(cli.timeout, lookup_host((host, port))).await {
        Err(_) => {
            let _ = writeln!(
                errors,
                "httping: DNS setup timed out after {}",
                humantime::format_duration(cli.timeout)
            );
            return ExitCode::FAILURE;
        }
        Ok(Err(error)) => {
            let _ = writeln!(errors, "httping: could not resolve {host}:{port}: {error}");
            return ExitCode::FAILURE;
        }
        Ok(Ok(addresses)) => {
            let mut addresses: Vec<SocketAddr> = addresses.collect();
            addresses.sort_unstable();
            addresses.dedup();
            if addresses.is_empty() {
                let _ = writeln!(
                    errors,
                    "httping: DNS returned no addresses for {host}:{port}"
                );
                return ExitCode::FAILURE;
            }
            addresses
        }
    };

    let client = match Client::builder()
        .user_agent(concat!("httping/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let _ = writeln!(errors, "httping: could not create HTTP client: {error}");
            return ExitCode::FAILURE;
        }
    };

    let _ = writeln!(output, "HTTPing {}", cli.url);
    for address in &addresses {
        let _ = writeln!(output, "resolved: {address}");
    }

    let mut statistics = Statistics::default();
    let mut next_start = TokioInstant::now();

    loop {
        if cli.count.is_some_and(|count| statistics.attempts >= count) {
            break;
        }

        tokio::select! {
            () = sleep_until(next_start) => {}
            signal = tokio::signal::ctrl_c() => {
                if let Err(error) = signal {
                    let _ = writeln!(errors, "httping: could not listen for Ctrl-C: {error}");
                    statistics.failed = true;
                }
                break;
            }
        }

        let probe_start = Instant::now();
        next_start = TokioInstant::now() + cli.interval;
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let request = client.get(cli.url.clone()).send();

        tokio::select! {
            result = timeout(cli.timeout, request) => {
                let elapsed = probe_start.elapsed();
                match result {
                    Ok(Ok(response)) => {
                        statistics.response(elapsed);
                        let _ = writeln!(
                            output,
                            "{} {} {}",
                            timestamp,
                            response.status(),
                            humantime::format_duration(elapsed)
                        );
                    }
                    Ok(Err(error)) => {
                        statistics.error();
                        let _ = writeln!(
                            output,
                            "{} error: {} {}",
                            timestamp,
                            error,
                            humantime::format_duration(elapsed)
                        );
                    }
                    Err(_) => {
                        statistics.error();
                        let _ = writeln!(
                            output,
                            "{} error: timed out after {} {}",
                            timestamp,
                            humantime::format_duration(cli.timeout),
                            humantime::format_duration(elapsed)
                        );
                    }
                }
            }
            signal = tokio::signal::ctrl_c() => {
                if let Err(error) = signal {
                    let _ = writeln!(errors, "httping: could not listen for Ctrl-C: {error}");
                    statistics.failed = true;
                }
                break;
            }
        }
    }

    let _ = write!(output, "{}", statistics.summary());
    if cli.count.is_some() && statistics.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn parses_defaults_and_duration_forms() {
        let cli = Cli::try_parse_from(["httping", "https://example.com"]).unwrap();
        assert_eq!(cli.count, None);
        assert_eq!(cli.interval, Duration::from_secs(1));
        assert_eq!(cli.timeout, Duration::from_secs(10));

        let cli = Cli::try_parse_from([
            "httping",
            "--count",
            "2",
            "--interval",
            "250ms",
            "--timeout",
            "2m",
            "http://example.com",
        ])
        .unwrap();
        assert_eq!(cli.count, Some(2));
        assert_eq!(cli.interval, Duration::from_millis(250));
        assert_eq!(cli.timeout, Duration::from_mins(2));
    }

    #[test]
    fn rejects_bad_command_lines() {
        for arguments in [
            vec!["httping", "--count", "0", "https://example.com"],
            vec!["httping", "ftp://example.com"],
            vec!["httping", "not-a-url"],
            vec!["httping", "https://example.com", "extra"],
        ] {
            assert_ne!(
                Cli::try_parse_from(arguments).unwrap_err().kind(),
                ErrorKind::DisplayHelp
            );
        }
    }

    #[test]
    fn summary_handles_responses_and_total_loss() {
        let mut statistics = Statistics::default();
        statistics.response(Duration::from_millis(10));
        statistics.response(Duration::from_millis(30));
        assert!(
            statistics
                .summary()
                .contains("attempts: 2, responses: 2, loss: 0.0%")
        );
        assert!(statistics.summary().contains("avg 20ms"));

        let mut statistics = Statistics::default();
        statistics.error();
        assert!(statistics.summary().contains("loss: 100.0%"));
        assert!(statistics.summary().contains("min n/a"));
    }

    #[tokio::test]
    async fn counts_non_successful_http_responses_and_times_header_wait() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });

        let cli = Cli::try_parse_from([
            "httping",
            "--count",
            "2",
            "--interval",
            "1ms",
            "--timeout",
            "1s",
            &format!("http://{address}/"),
        ])
        .unwrap();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let status = run_with_writers(cli, &mut output, &mut errors).await;
        server.await.unwrap();

        assert_eq!(status, ExitCode::SUCCESS);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("404 Not Found").count(), 2);
        assert!(output.contains("attempts: 2, responses: 2, loss: 0.0%"));
        assert!(output.contains("time: min 4"));
        assert!(errors.is_empty());
    }

    #[tokio::test]
    async fn continues_after_timeouts_and_returns_failure_for_finite_run() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (_stream, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });

        let cli = Cli::try_parse_from([
            "httping",
            "-c",
            "2",
            "-i",
            "1ms",
            "-t",
            "10ms",
            &format!("http://{address}/"),
        ])
        .unwrap();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let status = run_with_writers(cli, &mut output, &mut errors).await;
        server.await.unwrap();

        assert_eq!(status, ExitCode::FAILURE);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("error: timed out").count(), 2);
        assert!(output.contains("attempts: 2, responses: 0, loss: 100.0%"));
        assert!(errors.is_empty());
    }
}
