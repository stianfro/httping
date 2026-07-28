//! Command-line parsing, probing, and dashboard support for `HTTPing`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::Html;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Local, SecondsFormat};
use clap::{Arg, ArgMatches, Command};
use reqwest::{Client, Url};
use serde::Serialize;
use tokio::net::{TcpListener, lookup_host};
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant as TokioInstant, sleep_until, timeout};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

/// Ping an HTTP endpoint and report response latency.
#[derive(Debug)]
pub struct Cli {
    /// Selected operating mode.
    pub mode: Mode,

    /// Minimum time between probe starts.
    pub interval: Duration,

    /// Maximum time for DNS setup and for each probe.
    pub timeout: Duration,

    /// HTTP or HTTPS URL to probe.
    pub url: Url,
}

/// `HTTPing` operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write probe results to the terminal.
    Probe {
        /// Number of probes to send. By default, probes continue until Ctrl-C.
        count: Option<u64>,
    },
    /// Run the local web dashboard until Ctrl-C.
    Serve {
        /// Local dashboard port. Zero asks the operating system to select a port.
        port: u16,
        /// Do not open the dashboard in the default browser.
        no_open: bool,
        /// Maximum number of detailed probe events kept in memory.
        history: usize,
    },
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
        if let Some(("serve", serve)) = matches.subcommand() {
            return Self {
                mode: Mode::Serve {
                    port: *serve.get_one::<u16>("port").expect("port has a default"),
                    no_open: serve.get_flag("no-open"),
                    history: *serve
                        .get_one::<usize>("history")
                        .expect("history has a default"),
                },
                interval: required_value(serve, "interval"),
                timeout: required_value(serve, "timeout"),
                url: required_value::<Url>(serve, "url").clone(),
            };
        }

        Self {
            mode: Mode::Probe {
                count: matches.get_one::<u64>("count").copied(),
            },
            interval: required_value(matches, "interval"),
            timeout: required_value(matches, "timeout"),
            url: required_value::<Url>(matches, "url").clone(),
        }
    }
}

fn required_value<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, name: &str) -> T {
    matches
        .get_one::<T>(name)
        .unwrap_or_else(|| panic!("{name} is required or has a default"))
        .clone()
}

fn command() -> Command {
    Command::new("httping")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Ping an HTTP endpoint and report response latency")
        .subcommand_negates_reqs(true)
        .args(probe_arguments())
        .subcommand(
            Command::new("serve")
                .about("Show live probe statistics in a local web dashboard")
                .args(common_arguments())
                .arg(
                    Arg::new("port")
                        .long("port")
                        .value_name("PORT")
                        .help("Local dashboard port. By default, the operating system selects one")
                        .default_value("0")
                        .value_parser(clap::value_parser!(u16)),
                )
                .arg(
                    Arg::new("no-open")
                        .long("no-open")
                        .help("Do not open the dashboard in the default browser")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("history")
                        .long("history")
                        .value_name("COUNT")
                        .help("Number of recent probe results kept in memory")
                        .default_value("300")
                        .value_parser(parse_positive_usize),
                ),
        )
}

fn probe_arguments() -> Vec<Arg> {
    let mut arguments = common_arguments();
    arguments.insert(
        0,
        Arg::new("count")
            .short('c')
            .long("count")
            .value_name("COUNT")
            .help("Number of probes to send. By default, probes continue until Ctrl-C")
            .value_parser(clap::value_parser!(u64).range(1..)),
    );
    arguments
}

fn common_arguments() -> Vec<Arg> {
    vec![
        Arg::new("interval")
            .short('i')
            .long("interval")
            .value_name("DURATION")
            .help("Minimum time between probe starts")
            .default_value("1s")
            .value_parser(parse_duration),
        Arg::new("timeout")
            .short('t')
            .long("timeout")
            .value_name("DURATION")
            .help("Maximum time for DNS setup and for each probe")
            .default_value("10s")
            .value_parser(parse_duration),
        Arg::new("url")
            .value_name("URL")
            .help("HTTP or HTTPS URL to probe")
            .required(true)
            .value_parser(parse_url),
    ]
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let count = value.parse::<usize>().map_err(|error| error.to_string())?;
    if count == 0 {
        return Err("count must be greater than zero".to_owned());
    }
    Ok(count)
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

/// One completed HTTP probe.
#[derive(Clone, Debug, Serialize)]
pub struct ProbeEvent {
    /// Increasing sequence number for this run.
    pub sequence: u64,
    /// Probe start time in RFC 3339 format.
    pub timestamp: String,
    /// Time from request start until response headers or failure.
    pub elapsed_ms: f64,
    /// Result of the probe.
    #[serde(flatten)]
    pub outcome: ProbeOutcome,
}

/// Result data for a completed probe.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProbeOutcome {
    /// The endpoint returned HTTP response headers.
    Response {
        /// Numeric HTTP status code.
        status: u16,
        /// Display form of the HTTP status.
        status_text: String,
        /// Negotiated HTTP protocol version.
        protocol_version: String,
    },
    /// The request failed before response headers arrived.
    Error {
        /// Stable, low-cardinality error category.
        kind: ErrorKind,
        /// Human-readable error details.
        message: String,
    },
}

/// Stable probe transport error category.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The probe exceeded its configured timeout.
    Timeout,
    /// The connection could not be established.
    Connect,
    /// A redirect could not be followed.
    Redirect,
    /// A request construction or transmission error occurred.
    Request,
    /// Another transport error occurred.
    Other,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Statistics {
    attempts: u64,
    responses: u64,
    loss_percent: f64,
    minimum_ms: Option<f64>,
    average_ms: Option<f64>,
    maximum_ms: Option<f64>,
    latest_ms: Option<f64>,
    #[serde(skip)]
    total: Duration,
    #[serde(skip)]
    minimum: Option<Duration>,
    #[serde(skip)]
    maximum: Option<Duration>,
    #[serde(skip)]
    failed: bool,
}

impl Statistics {
    fn record(&mut self, event: &ProbeEvent) {
        self.attempts += 1;
        let elapsed = Duration::from_secs_f64(event.elapsed_ms / 1_000.0);
        self.latest_ms = Some(event.elapsed_ms);
        if matches!(event.outcome, ProbeOutcome::Response { .. }) {
            self.responses += 1;
            self.total += elapsed;
            self.minimum = Some(self.minimum.map_or(elapsed, |value| value.min(elapsed)));
            self.maximum = Some(self.maximum.map_or(elapsed, |value| value.max(elapsed)));
            self.minimum_ms = self.minimum.map(duration_millis);
            self.maximum_ms = self.maximum.map(duration_millis);
            self.average_ms = Some(duration_millis(
                self.total / u32::try_from(self.responses).unwrap_or(u32::MAX),
            ));
        } else {
            self.failed = true;
        }
        self.loss_percent = if self.attempts == 0 {
            0.0
        } else {
            let loss_tenths =
                (u128::from(self.attempts - self.responses) * 1_000) / u128::from(self.attempts);
            f64::from(u32::try_from(loss_tenths).unwrap_or(1_000)) / 10.0
        };
    }

    fn summary(&self) -> String {
        let mut output = format!(
            "\n--- httping statistics ---\nattempts: {}, responses: {}, loss: {:.1}%\n",
            self.attempts, self.responses, self.loss_percent
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

fn duration_millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

struct Prepared {
    client: Client,
    addresses: Vec<SocketAddr>,
}

async fn prepare(cli: &Cli, errors: &mut impl Write) -> Result<Prepared, ExitCode> {
    let Some(host) = cli.url.host_str() else {
        let _ = writeln!(errors, "httping: URL has no hostname");
        return Err(ExitCode::FAILURE);
    };
    let Some(port) = cli.url.port_or_known_default() else {
        let _ = writeln!(errors, "httping: URL has no effective port");
        return Err(ExitCode::FAILURE);
    };

    let addresses = match timeout(cli.timeout, lookup_host((host, port))).await {
        Err(_) => {
            let _ = writeln!(
                errors,
                "httping: DNS setup timed out after {}",
                humantime::format_duration(cli.timeout)
            );
            return Err(ExitCode::FAILURE);
        }
        Ok(Err(error)) => {
            let _ = writeln!(errors, "httping: could not resolve {host}:{port}: {error}");
            return Err(ExitCode::FAILURE);
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
                return Err(ExitCode::FAILURE);
            }
            addresses
        }
    };

    let client = Client::builder()
        .user_agent(concat!("httping/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| {
            let _ = writeln!(errors, "httping: could not create HTTP client: {error}");
            ExitCode::FAILURE
        })?;

    Ok(Prepared { client, addresses })
}

fn write_header(cli: &Cli, prepared: &Prepared, output: &mut impl Write) {
    let _ = writeln!(output, "HTTPing {}", cli.url);
    for address in &prepared.addresses {
        let _ = writeln!(output, "resolved: {address}");
    }
}

fn write_event(event: &ProbeEvent, output: &mut impl Write) {
    match &event.outcome {
        ProbeOutcome::Response { status_text, .. } => {
            let _ = writeln!(
                output,
                "{} {} {}",
                event.timestamp,
                status_text,
                humantime::format_duration(Duration::from_secs_f64(event.elapsed_ms / 1_000.0))
            );
        }
        ProbeOutcome::Error { message, .. } => {
            let _ = writeln!(
                output,
                "{} error: {} {}",
                event.timestamp,
                message,
                humantime::format_duration(Duration::from_secs_f64(event.elapsed_ms / 1_000.0))
            );
        }
    }
}

struct RunResult {
    statistics: Statistics,
    interrupted: bool,
}

async fn probe_loop(
    cli: &Cli,
    prepared: &Prepared,
    count: Option<u64>,
    mut observe: impl FnMut(&ProbeEvent),
    errors: &mut impl Write,
) -> RunResult {
    let mut statistics = Statistics::default();
    let mut next_start = TokioInstant::now();
    let mut interrupted = false;

    loop {
        if count.is_some_and(|limit| statistics.attempts >= limit) {
            break;
        }
        tokio::select! {
            () = sleep_until(next_start) => {}
            signal = tokio::signal::ctrl_c() => {
                if let Err(error) = signal {
                    let _ = writeln!(errors, "httping: could not listen for Ctrl-C: {error}");
                    statistics.failed = true;
                } else {
                    interrupted = true;
                }
                break;
            }
        }

        let probe_start = Instant::now();
        next_start = TokioInstant::now() + cli.interval;
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let request = prepared.client.get(cli.url.clone()).send();
        let result = tokio::select! {
            result = timeout(cli.timeout, request) => Some(result),
            signal = tokio::signal::ctrl_c() => {
                if let Err(error) = signal {
                    let _ = writeln!(errors, "httping: could not listen for Ctrl-C: {error}");
                    statistics.failed = true;
                } else {
                    interrupted = true;
                }
                None
            }
        };
        let Some(result) = result else { break };
        let elapsed = probe_start.elapsed();
        let outcome = match result {
            Ok(Ok(response)) => ProbeOutcome::Response {
                status: response.status().as_u16(),
                status_text: response.status().to_string(),
                protocol_version: format!("{:?}", response.version()),
            },
            Ok(Err(error)) => ProbeOutcome::Error {
                kind: classify_error(&error),
                message: error.to_string(),
            },
            Err(_) => ProbeOutcome::Error {
                kind: ErrorKind::Timeout,
                message: format!(
                    "timed out after {}",
                    humantime::format_duration(cli.timeout)
                ),
            },
        };
        let event = ProbeEvent {
            sequence: statistics.attempts + 1,
            timestamp,
            elapsed_ms: duration_millis(elapsed),
            outcome,
        };
        statistics.record(&event);
        observe(&event);
    }

    RunResult {
        statistics,
        interrupted,
    }
}

fn classify_error(error: &reqwest::Error) -> ErrorKind {
    if error.is_timeout() {
        ErrorKind::Timeout
    } else if error.is_connect() {
        ErrorKind::Connect
    } else if error.is_redirect() {
        ErrorKind::Redirect
    } else if error.is_request() {
        ErrorKind::Request
    } else {
        ErrorKind::Other
    }
}

/// Run `HTTPing` and return its process exit status.
pub async fn run(cli: Cli) -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_with_writers(cli, &mut stdout.lock(), &mut stderr.lock()).await
}

async fn run_with_writers(cli: Cli, output: &mut impl Write, errors: &mut impl Write) -> ExitCode {
    let prepared = match prepare(&cli, errors).await {
        Ok(prepared) => prepared,
        Err(status) => return status,
    };
    write_header(&cli, &prepared, output);

    match cli.mode {
        Mode::Probe { count } => {
            let result = probe_loop(
                &cli,
                &prepared,
                count,
                |event| write_event(event, output),
                errors,
            )
            .await;
            let _ = write!(output, "{}", result.statistics.summary());
            if count.is_some() && result.statistics.failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Mode::Serve {
            port,
            no_open,
            history,
        } => run_dashboard(&cli, &prepared, port, no_open, history, output, errors).await,
    }
}

#[derive(Clone, Debug, Serialize)]
struct DashboardState {
    target: String,
    started_at: String,
    interval_ms: f64,
    timeout_ms: f64,
    resolved_addresses: Vec<String>,
    statistics: Statistics,
    history: VecDeque<ProbeEvent>,
    last_sequence: u64,
    history_limit: usize,
}

impl DashboardState {
    fn new(cli: &Cli, prepared: &Prepared, history_limit: usize) -> Self {
        Self {
            target: safe_target_name(&cli.url),
            started_at: Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            interval_ms: duration_millis(cli.interval),
            timeout_ms: duration_millis(cli.timeout),
            resolved_addresses: prepared.addresses.iter().map(ToString::to_string).collect(),
            statistics: Statistics::default(),
            history: VecDeque::with_capacity(history_limit.min(4_096)),
            last_sequence: 0,
            history_limit,
        }
    }

    fn record(&mut self, event: &ProbeEvent) {
        self.statistics.record(event);
        self.last_sequence = event.sequence;
        self.history.push_back(event.clone());
        while self.history.len() > self.history_limit {
            self.history.pop_front();
        }
    }
}

#[derive(Clone)]
struct WebState {
    dashboard: Arc<RwLock<DashboardState>>,
    events: broadcast::Sender<ProbeEvent>,
}

async fn dashboard_page() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn dashboard_state(State(state): State<WebState>) -> Json<DashboardState> {
    Json(
        state
            .dashboard
            .read()
            .expect("dashboard lock is not poisoned")
            .clone(),
    )
}

async fn dashboard_events(
    State(state): State<WebState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|result| {
        result.ok().and_then(|event| {
            serde_json::to_string(&event).ok().map(|data| {
                Ok(SseEvent::default()
                    .id(event.sequence.to_string())
                    .event("probe")
                    .data(data))
            })
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn run_dashboard(
    cli: &Cli,
    prepared: &Prepared,
    port: u16,
    no_open: bool,
    history: usize,
    output: &mut impl Write,
    errors: &mut impl Write,
) -> ExitCode {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = writeln!(errors, "httping: could not bind dashboard: {error}");
            return ExitCode::FAILURE;
        }
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            let _ = writeln!(errors, "httping: could not read dashboard address: {error}");
            return ExitCode::FAILURE;
        }
    };
    let dashboard = Arc::new(RwLock::new(DashboardState::new(cli, prepared, history)));
    let (events, _) = broadcast::channel(history.clamp(16, 4_096));
    let web_state = WebState {
        dashboard: Arc::clone(&dashboard),
        events: events.clone(),
    };
    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/api/state", get(dashboard_state))
        .route("/api/events", get(dashboard_events))
        .with_state(web_state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let url = format!("http://{address}/");
    let _ = writeln!(output, "dashboard: {url}");
    if !no_open && open::that_detached(&url).is_err() {
        let _ = writeln!(errors, "httping: could not open the dashboard in a browser");
    }

    let result = probe_loop(
        cli,
        prepared,
        None,
        |event| {
            write_event(event, output);
            dashboard
                .write()
                .expect("dashboard lock is not poisoned")
                .record(event);
            let _ = events.send(event.clone());
        },
        errors,
    )
    .await;
    let _ = shutdown_tx.send(());
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = writeln!(errors, "httping: dashboard server failed: {error}");
        }
        Err(error) => {
            let _ = writeln!(errors, "httping: dashboard task failed: {error}");
        }
    }
    let _ = write!(output, "{}", result.statistics.summary());
    let _ = result.interrupted;
    ExitCode::SUCCESS
}

fn safe_target_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    let port = url.port_or_known_default().unwrap_or(0);
    format!("{}://{host}:{port}{}", url.scheme(), url.path())
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>HTTPing dashboard</title><style>
:root{color-scheme:dark;font-family:ui-sans-serif,system-ui,sans-serif;background:#0b1020;color:#e8eefc}body{margin:0;padding:24px}.wrap{max-width:1100px;margin:auto}h1{margin:0}.target{color:#9fb0d0;overflow-wrap:anywhere}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:12px;margin:24px 0}.card,.panel{background:#151d33;border:1px solid #26314f;border-radius:12px;padding:16px}.label{font-size:.8rem;color:#9fb0d0;text-transform:uppercase}.value{font-size:1.7rem;font-weight:700;margin-top:6px}canvas{width:100%;height:260px}table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:9px;border-bottom:1px solid #26314f}th{color:#9fb0d0}.ok{color:#65d394}.error{color:#ff7b86}.status{float:right;font-size:.85rem}.live::before{content:'●';color:#65d394;margin-right:6px}@media(max-width:600px){body{padding:12px}.panel{overflow-x:auto}}
</style></head><body><main class="wrap"><span id="connection" class="status">Connecting</span><h1>HTTPing</h1><p id="target" class="target"></p><section class="grid" id="cards"></section><section class="panel"><h2>Latency</h2><canvas id="chart" width="1000" height="260"></canvas></section><section class="panel" style="margin-top:12px"><h2>Recent probes</h2><table><thead><tr><th>#</th><th>Time</th><th>Result</th><th>Latency</th></tr></thead><tbody id="results"></tbody></table></section></main>
<script>
let state=null;const cards=[['attempts','Attempts'],['responses','Responses'],['loss_percent','Loss','%'],['latest_ms','Latest',' ms'],['minimum_ms','Minimum',' ms'],['average_ms','Average',' ms'],['maximum_ms','Maximum',' ms']];
const fmt=v=>v==null?'n/a':Number(v).toFixed(1);function render(){if(!state)return;document.querySelector('#target').textContent=state.target;document.querySelector('#cards').innerHTML=cards.map(([k,l,u=''])=>`<div class="card"><div class="label">${l}</div><div class="value">${fmt(state.statistics[k])}${u}</div></div>`).join('');const rows=[...state.history].reverse().slice(0,50);document.querySelector('#results').innerHTML=rows.map(e=>{const ok=e.type==='response';return `<tr><td>${e.sequence}</td><td>${e.timestamp}</td><td class="${ok?'ok':'error'}">${ok?e.status_text:e.message}</td><td>${fmt(e.elapsed_ms)} ms</td></tr>`}).join('');draw();}
function draw(){const c=document.querySelector('#chart'),x=c.getContext('2d'),d=state.history;x.clearRect(0,0,c.width,c.height);x.strokeStyle='#26314f';x.beginPath();x.moveTo(45,10);x.lineTo(45,230);x.lineTo(990,230);x.stroke();if(!d.length)return;const max=Math.max(1,...d.map(e=>e.elapsed_ms));d.forEach((e,i)=>{const px=50+i*Math.max(1,935/Math.max(1,d.length-1)),py=225-e.elapsed_ms/max*205;x.fillStyle=e.type==='response'?'#65d394':'#ff7b86';x.beginPath();x.arc(px,py,3,0,Math.PI*2);x.fill();if(i){const p=d[i-1];x.strokeStyle='#6e8bc4';x.beginPath();x.moveTo(50+(i-1)*Math.max(1,935/Math.max(1,d.length-1)),225-p.elapsed_ms/max*205);x.lineTo(px,py);x.stroke();}});x.fillStyle='#9fb0d0';x.fillText(max.toFixed(1)+' ms',2,16);}
async function load(){state=await fetch('/api/state').then(r=>r.json());render();}function connect(){const es=new EventSource('/api/events');es.onopen=()=>{const e=document.querySelector('#connection');e.textContent='Live';e.className='status live'};es.onmessage=()=>{};es.addEventListener('probe',async m=>{const e=JSON.parse(m.data);if(state&&e.sequence!==state.last_sequence+1){await load();return}state.history.push(e);while(state.history.length>state.history_limit)state.history.shift();state.last_sequence=e.sequence;await load();});es.onerror=()=>{const e=document.querySelector('#connection');e.textContent='Reconnecting';e.className='status error'};}load().then(connect);
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind as ClapErrorKind;

    #[test]
    fn parses_defaults_and_duration_forms() {
        let cli = Cli::try_parse_from(["httping", "https://example.com"]).unwrap();
        assert_eq!(cli.mode, Mode::Probe { count: None });
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
        assert_eq!(cli.mode, Mode::Probe { count: Some(2) });
        assert_eq!(cli.interval, Duration::from_millis(250));
        assert_eq!(cli.timeout, Duration::from_mins(2));
    }

    #[test]
    fn parses_dashboard_options() {
        let cli = Cli::try_parse_from([
            "httping",
            "serve",
            "--port",
            "8080",
            "--no-open",
            "--history",
            "10",
            "--interval",
            "250ms",
            "https://example.com/private?token=secret",
        ])
        .unwrap();
        assert_eq!(
            cli.mode,
            Mode::Serve {
                port: 8080,
                no_open: true,
                history: 10
            }
        );
        assert_eq!(
            safe_target_name(&cli.url),
            "https://example.com:443/private"
        );
    }

    #[test]
    fn rejects_bad_command_lines() {
        for arguments in [
            vec!["httping", "--count", "0", "https://example.com"],
            vec!["httping", "ftp://example.com"],
            vec!["httping", "not-a-url"],
            vec!["httping", "https://example.com", "extra"],
            vec!["httping", "serve", "--history", "0", "https://example.com"],
        ] {
            assert_ne!(
                Cli::try_parse_from(arguments).unwrap_err().kind(),
                ClapErrorKind::DisplayHelp
            );
        }
    }

    #[test]
    fn dashboard_history_is_bounded_but_statistics_are_not() {
        let cli = Cli::try_parse_from(["httping", "https://example.com"]).unwrap();
        let prepared = Prepared {
            client: Client::new(),
            addresses: vec!["127.0.0.1:443".parse().unwrap()],
        };
        let mut state = DashboardState::new(&cli, &prepared, 2);
        for (sequence, elapsed_ms) in [(1, 1.0), (2, 2.0), (3, 3.0)] {
            state.record(&ProbeEvent {
                sequence,
                timestamp: "2026-01-01T00:00:00Z".to_owned(),
                elapsed_ms,
                outcome: ProbeOutcome::Response {
                    status: 200,
                    status_text: "200 OK".to_owned(),
                    protocol_version: "HTTP/1.1".to_owned(),
                },
            });
        }
        assert_eq!(state.statistics.attempts, 3);
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history.front().unwrap().sequence, 2);
    }

    #[test]
    fn summary_handles_responses_and_total_loss() {
        let mut statistics = Statistics::default();
        for (sequence, elapsed_ms) in [(1, 10.0), (2, 30.0)] {
            statistics.record(&ProbeEvent {
                sequence,
                timestamp: String::new(),
                elapsed_ms,
                outcome: ProbeOutcome::Response {
                    status: 200,
                    status_text: "200 OK".to_owned(),
                    protocol_version: "HTTP/1.1".to_owned(),
                },
            });
        }
        assert!(
            statistics
                .summary()
                .contains("attempts: 2, responses: 2, loss: 0.0%")
        );
        assert!(statistics.summary().contains("avg 20ms"));
    }

    #[tokio::test]
    async fn counts_non_successful_http_responses_and_times_header_wait() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
