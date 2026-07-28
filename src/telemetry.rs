//! Optional OpenTelemetry metric export.

use std::env;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;

use crate::{ErrorKind, ProbeEvent, ProbeOutcome};

const DURATION_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

#[derive(Debug)]
struct ReportingExporter {
    inner: opentelemetry_otlp::MetricExporter,
}

impl PushMetricExporter for ReportingExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        if let Err(error) = &result {
            eprintln!("httping: background OTLP metric export failed: {error}");
        }
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

pub(crate) struct Telemetry {
    provider: SdkMeterProvider,
    duration: Histogram<f64>,
    attempts: Counter<u64>,
    responses: Counter<u64>,
    transport_errors: Counter<u64>,
    base_attributes: Vec<KeyValue>,
}

impl Telemetry {
    pub(crate) fn from_environment(
        enabled_by_flag: bool,
        target_name: Option<&str>,
        url: &reqwest::Url,
    ) -> Result<Option<Self>, String> {
        let exporter = env::var("OTEL_METRICS_EXPORTER").ok();
        if !telemetry_enabled(enabled_by_flag, exporter.as_deref())? {
            return Ok(None);
        }
        validate_protocol(
            env::var("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
                .ok()
                .or_else(|| env::var("OTEL_EXPORTER_OTLP_PROTOCOL").ok())
                .as_deref(),
        )?;
        Self::build(target_name, url, None).map(Some)
    }

    fn build(
        target_name: Option<&str>,
        url: &reqwest::Url,
        endpoint: Option<&str>,
    ) -> Result<Self, String> {
        let exporter_builder = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary);
        let exporter = if let Some(endpoint) = endpoint {
            exporter_builder.with_endpoint(endpoint).build()
        } else {
            exporter_builder.build()
        }
        .map_err(|error| format!("could not configure OTLP metrics: {error}"))?;
        let exporter = ReportingExporter { inner: exporter };

        let mut resource = Resource::builder();
        if !environment_has_service_name() {
            resource = resource.with_service_name("httping");
        }
        if !environment_has_resource_attribute("service.version") {
            resource = resource
                .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
        }
        let provider = SdkMeterProvider::builder()
            .with_resource(resource.build())
            .with_periodic_exporter(exporter)
            .build();
        let scope = InstrumentationScope::builder("httping")
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let meter = provider.meter_with_scope(scope);
        let duration = meter
            .f64_histogram("http.client.request.duration")
            .with_description("Time until HTTP response headers or a transport failure")
            .with_unit("s")
            .with_boundaries(DURATION_BUCKETS.to_vec())
            .build();
        let attempts = meter
            .u64_counter("httping.probe.attempts")
            .with_description("Number of HTTP probes started")
            .with_unit("{probe}")
            .build();
        let responses = meter
            .u64_counter("httping.probe.responses")
            .with_description("Number of HTTP responses received")
            .with_unit("{response}")
            .build();
        let transport_errors = meter
            .u64_counter("httping.probe.transport_errors")
            .with_description("Number of HTTP probe transport failures")
            .with_unit("{error}")
            .build();

        let host = url.host_str().unwrap_or("unknown").to_owned();
        let port = i64::from(url.port_or_known_default().unwrap_or(0));
        let name = target_name.map_or_else(|| crate::safe_target_name(url), ToOwned::to_owned);
        let base_attributes = vec![
            KeyValue::new("http.request.method", "GET"),
            KeyValue::new("server.address", host),
            KeyValue::new("server.port", port),
            KeyValue::new("url.scheme", url.scheme().to_owned()),
            KeyValue::new("httping.target.name", name),
        ];

        Ok(Self {
            provider,
            duration,
            attempts,
            responses,
            transport_errors,
            base_attributes,
        })
    }

    pub(crate) fn record(&self, event: &ProbeEvent) {
        self.attempts.add(1, &self.base_attributes);
        let mut attributes = self.base_attributes.clone();
        match &event.outcome {
            ProbeOutcome::Response {
                status,
                protocol_version,
                ..
            } => {
                attributes.push(KeyValue::new(
                    "http.response.status_code",
                    i64::from(*status),
                ));
                attributes.push(KeyValue::new(
                    "network.protocol.version",
                    protocol_version_value(protocol_version),
                ));
                if *status >= 500 {
                    attributes.push(KeyValue::new("error.type", status.to_string()));
                }
                self.responses.add(1, &attributes);
            }
            ProbeOutcome::Error { kind, .. } => {
                attributes.push(KeyValue::new("error.type", error_type(*kind)));
                self.transport_errors.add(1, &attributes);
            }
        }
        self.duration
            .record(event.elapsed_ms / 1_000.0, &attributes);
    }

    pub(crate) fn finish(self) -> Vec<String> {
        let mut errors = Vec::new();
        if let Err(error) = self.provider.force_flush() {
            errors.push(format!("could not flush OTLP metrics: {error}"));
        }
        if let Err(error) = self.provider.shutdown() {
            errors.push(format!("could not shut down OTLP metrics: {error}"));
        }
        errors
    }
}

fn telemetry_enabled(flag: bool, exporter: Option<&str>) -> Result<bool, String> {
    if flag {
        return Ok(true);
    }
    match exporter.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(false),
        Some(value) if value.eq_ignore_ascii_case("none") => Ok(false),
        Some(value) if value.eq_ignore_ascii_case("otlp") => Ok(true),
        Some(value) => Err(format!(
            "unsupported OTEL_METRICS_EXPORTER value {value:?}; use otlp or none"
        )),
    }
}

fn validate_protocol(protocol: Option<&str>) -> Result<(), String> {
    match protocol.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(()),
        Some(value) if value.eq_ignore_ascii_case("http/protobuf") => Ok(()),
        Some(value) => Err(format!(
            "unsupported OTLP metrics protocol {value:?}; use http/protobuf"
        )),
    }
}

fn environment_has_service_name() -> bool {
    if env::var("OTEL_SERVICE_NAME").is_ok_and(|value| !value.trim().is_empty()) {
        return true;
    }
    environment_has_resource_attribute("service.name")
}

fn environment_has_resource_attribute(name: &str) -> bool {
    env::var("OTEL_RESOURCE_ATTRIBUTES").is_ok_and(|attributes| {
        attributes.split(',').any(|attribute| {
            attribute
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == name)
        })
    })
}

fn protocol_version_value(version: &str) -> String {
    version.strip_prefix("HTTP/").unwrap_or(version).to_owned()
}

fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Timeout => "timeout",
        ErrorKind::Connect => "connect",
        ErrorKind::Redirect => "redirect",
        ErrorKind::Request => "request",
        ErrorKind::Other => "_OTHER",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_requires_explicit_enablement() {
        assert!(!telemetry_enabled(false, None).unwrap());
        assert!(!telemetry_enabled(false, Some("")).unwrap());
        assert!(!telemetry_enabled(false, Some("none")).unwrap());
        assert!(!telemetry_enabled(false, Some("NONE")).unwrap());
        assert!(telemetry_enabled(false, Some("OTLP")).unwrap());
        assert!(telemetry_enabled(true, Some("none")).unwrap());
        assert!(telemetry_enabled(false, Some("prometheus")).is_err());
    }

    #[test]
    fn only_http_protobuf_is_valid() {
        assert!(validate_protocol(None).is_ok());
        assert!(validate_protocol(Some("http/protobuf")).is_ok());
        assert!(validate_protocol(Some("grpc")).is_err());
        assert!(validate_protocol(Some("http/json")).is_err());
    }

    #[test]
    fn metric_attribute_values_are_stable() {
        assert_eq!(protocol_version_value("HTTP/1.1"), "1.1");
        assert_eq!(error_type(ErrorKind::Timeout), "timeout");
        assert_eq!(error_type(ErrorKind::Other), "_OTHER");
    }

    #[test]
    fn builds_instruments_without_contacting_the_collector() {
        let url = reqwest::Url::parse("https://user:secret@example.com/a?token=x").unwrap();
        let telemetry =
            Telemetry::build(None, &url, Some("http://127.0.0.1:9/v1/metrics")).unwrap();
        let target = telemetry
            .base_attributes
            .iter()
            .find(|value| value.key.as_str() == "httping.target.name")
            .unwrap();
        assert_eq!(target.value.as_str(), "https://example.com:443/a");
    }

    #[test]
    fn exports_otlp_protobuf_metrics() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let request = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&request);
        let collector = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = vec![0; 32_768];
                let size = stream.read(&mut buffer).unwrap();
                captured.lock().unwrap().extend_from_slice(&buffer[..size]);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        });

        let url = reqwest::Url::parse("https://example.com/health").unwrap();
        let telemetry = Telemetry::build(
            Some("production-api"),
            &url,
            Some(&format!("http://{address}/v1/metrics")),
        )
        .unwrap();
        telemetry.record(&ProbeEvent {
            sequence: 1,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            elapsed_ms: 42.0,
            outcome: ProbeOutcome::Response {
                status: 204,
                status_text: "204 No Content".to_owned(),
                protocol_version: "HTTP/1.1".to_owned(),
            },
        });
        telemetry.record(&ProbeEvent {
            sequence: 2,
            timestamp: "2026-01-01T00:00:01Z".to_owned(),
            elapsed_ms: 100.0,
            outcome: ProbeOutcome::Error {
                kind: ErrorKind::Timeout,
                message: "secret local error details".to_owned(),
            },
        });
        let errors = telemetry.finish();
        assert!(errors.is_empty(), "{errors:?}");
        collector.join().unwrap();

        let request = request.lock().unwrap();
        for value in [
            b"POST /v1/metrics HTTP/1.1".as_slice(),
            b"application/x-protobuf",
            b"http.client.request.duration",
            b"httping.probe.attempts",
            b"httping.probe.responses",
            b"httping.probe.transport_errors",
            b"production-api",
        ] {
            assert!(request.windows(value.len()).any(|window| window == value));
        }
        assert!(
            !request
                .windows(b"secret local error details".len())
                .any(|window| window == b"secret local error details")
        );
    }
}
