// Copyright 2022, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{
    common::{d_true, OtelDefaults},
    logs, metrics, trace,
};
use crate::connectors::prelude::*;
use tonic::transport::Channel as TonicChannel;
use tonic::transport::Endpoint as TonicEndpoint;
use tremor_otelapis::opentelemetry::proto::collector::{
    logs::v1::{logs_service_client::LogsServiceClient, ExportLogsServiceRequest},
    metrics::v1::{metrics_service_client::MetricsServiceClient, ExportMetricsServiceRequest},
    trace::v1::{trace_service_client::TraceServiceClient, ExportTraceServiceRequest},
};

const CONNECTOR_TYPE: &str = "otel_client";

// TODO Consider concurrency cap?

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct Config {
    /// The hostname or IP address for the remote OpenTelemetry collector endpoint
    #[serde(default = "Default::default")]
    pub(crate) url: Url<OtelDefaults>,
    #[serde(default = "d_true")]
    pub(crate) logs: bool,
    /// Enables the trace service
    #[serde(default = "d_true")]
    pub(crate) trace: bool,
    /// Enables the metrics service
    #[serde(default = "d_true")]
    pub(crate) metrics: bool,
}

impl ConfigImpl for Config {}

fn json_otel_logs_to_pb(json: &Value<'_>) -> Result<ExportLogsServiceRequest> {
    let pb = ExportLogsServiceRequest {
        resource_logs: logs::resource_logs_to_pb(json)?,
    };
    Ok(pb)
}

fn json_otel_trace_to_pb(json: &Value<'_>) -> Result<ExportTraceServiceRequest> {
    let pb = ExportTraceServiceRequest {
        resource_spans: trace::resource_spans_to_pb(Some(json))?,
    };
    Ok(pb)
}

fn json_otel_metrics_to_pb(json: &Value<'_>) -> Result<ExportMetricsServiceRequest> {
    let pb = ExportMetricsServiceRequest {
        resource_metrics: metrics::resource_metrics_to_pb(Some(json))?,
    };
    Ok(pb)
}

/// The `OpenTelemetry` client connector
pub(crate) struct Client {
    config: Config,
    origin_uri: EventOriginUri,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OtelClient")
    }
}

#[derive(Debug, Default)]
pub(crate) struct Builder {}

#[async_trait::async_trait]
impl ConnectorBuilder for Builder {
    fn connector_type(&self) -> ConnectorType {
        CONNECTOR_TYPE.into()
    }

    async fn from_config(
        &self,
        _id: &str,
        connector_config: &ConnectorConfig,
    ) -> Result<Box<dyn Connector>> {
        if let Some(config) = &connector_config.config {
            let config = Config::new(config)?;
            let origin_uri = EventOriginUri {
                scheme: "tremor-otel-client".to_string(),
                host: config.url.host_or_local().to_string(),
                port: config.url.port(),
                path: vec![],
            };

            Ok(Box::new(Client { config, origin_uri }))
        } else {
            Err(ErrorKind::MissingConfiguration(String::from("OtelClient")).into())
        }
    }
}

#[derive(Clone)]
pub(crate) struct RemoteOpenTelemetryEndpoint {
    logs_client: LogsServiceClient<TonicChannel>,
    metrics_client: MetricsServiceClient<TonicChannel>,
    trace_client: TraceServiceClient<TonicChannel>,
}

#[async_trait::async_trait]
impl Connector for Client {
    fn codec_requirements(&self) -> CodecReq {
        CodecReq::Structured
    }

    async fn create_sink(
        &mut self,
        sink_context: SinkContext,
        builder: SinkManagerBuilder,
    ) -> Result<Option<SinkAddr>> {
        let sink = OtelSink {
            origin_uri: self.origin_uri.clone(),
            config: self.config.clone(),
            remote: None,
        };
        builder.spawn(sink, sink_context).map(Some)
    }
}

#[allow(dead_code)]
struct OtelSink {
    origin_uri: EventOriginUri,
    config: Config,
    remote: Option<RemoteOpenTelemetryEndpoint>,
}

#[async_trait::async_trait()]
impl Sink for OtelSink {
    async fn connect(&mut self, _ctx: &SinkContext, _attempt: &Attempt) -> Result<bool> {
        let endpoint = self.config.url.to_string();
        let channel = TonicEndpoint::from_shared(endpoint)
            .map_err(|e| format!("Unable to connect to remote otel endpoint: {}", e))?
            .connect()
            .await?;

        let logs_client = LogsServiceClient::new(channel.clone());
        let metrics_client = MetricsServiceClient::new(channel.clone());
        let trace_client = TraceServiceClient::new(channel);

        self.remote = Some(RemoteOpenTelemetryEndpoint {
            logs_client,
            metrics_client,
            trace_client,
        });

        Ok(true)
    }
    async fn on_event(
        &mut self,
        _input: &str,
        event: Event,
        ctx: &SinkContext,
        _serializer: &mut EventSerializer,
        _start: u64,
    ) -> Result<SinkReply> {
        if let Some(remote) = &mut self.remote {
            // Up
            for value in event.value_iter() {
                let o = value
                    .as_object()
                    .ok_or_else(|| format!("Unsupported event received by OTEL sink: {}", value))?;
                if o.contains_key("metrics") {
                    if self.config.metrics {
                        let request = json_otel_metrics_to_pb(value)?;
                        if let Err(e) = remote.metrics_client.export(request).await {
                            error!("Failed to dispatch otel/gRPC metrics message: {}", e);
                            ctx.notifier().connection_lost().await?;
                            return Ok(SinkReply::fail_or_none(false));
                        };
                        continue;
                    }
                    warn!("Otel Source received metrics event when metrics support is disabled. Dropping trace");
                } else if o.contains_key("logs") {
                    if self.config.logs {
                        let request = json_otel_logs_to_pb(value)?;
                        if let Err(e) = remote.logs_client.export(request).await {
                            error!("Failed to dispatch otel/gRPC logs message: {}", e);
                            ctx.notifier().connection_lost().await?;
                            return Ok(SinkReply::fail_or_none(false));
                        }
                        continue;
                    }
                    warn!(
                        "Otel Sink received log event when log support is disabled. Dropping trace"
                    );
                } else if o.contains_key("trace") {
                    if self.config.trace {
                        let request = json_otel_trace_to_pb(value)?;
                        if let Err(e) = remote.trace_client.export(request).await {
                            error!("Failed to dispatch otel/gRPC logs message: {}", e);
                            ctx.notifier().connection_lost().await?;
                            return Ok(SinkReply::fail_or_none(event.transactional));
                        }
                        continue;
                    }
                    warn!("Otel Sink received trace event when trace support is disabled. Dropping trace");
                }
            }

            Ok(SinkReply::ack_or_none(event.transactional))
        } else {
            error!("{ctx} Sending to a non connected sink!");
            ctx.notifier().connection_lost().await?;
            Ok(SinkReply::fail_or_none(event.transactional))
        }
    }

    async fn on_signal(
        &mut self,
        _signal: Event,
        _ctx: &SinkContext,
        _serializer: &mut EventSerializer,
    ) -> Result<SinkReply> {
        Ok(SinkReply::default())
    }

    fn auto_ack(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[async_std::test]
    async fn otel_client_builder() -> Result<()> {
        let with_processors = literal!({
            "id": "my_otel_client",
            "type": "otel_client",
            "config": {
                "host": "localhost",
                "port": 4317,
            },
        });
        let config: ConnectorConfig = crate::config::Connector::from_config(
            ConnectorType("otel_client".into()),
            &with_processors,
        )?;

        let builder = super::Builder::default();
        let _connector = builder.from_config("foo", &config).await?;

        Ok(())
    }
}