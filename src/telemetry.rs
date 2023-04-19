use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue},
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Metric,
};
use tracing::subscriber::set_global_default;
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EncodeLabelValue)]
pub enum ConnectionRole {
    Server,
    Client,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ProxyConnectionLabels {
    role: ConnectionRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EncodeLabelValue)]
pub enum AssetRequestResult {
    Success,
    Malformed,
    AllocationNotFound,
    AssetNotFound,
    Error,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct AssetRequestLabels {
    result: AssetRequestResult,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    connections: Family<ProxyConnectionLabels, Counter>,
    current_connections: Family<ProxyConnectionLabels, Gauge>,
    asset_requests: Family<AssetRequestLabels, Counter>,
    current_assets_size_bytes: Gauge,
    asset_response_size_bytes: Histogram,
}

fn register<N: Into<String>, H: Into<String>, M: Metric + Clone + Default>(
    registry: &mut prometheus_client::registry::Registry,
    name: N,
    help: H,
) -> M {
    register_metric(registry, name, help, M::default())
}

fn register_metric<N: Into<String>, H: Into<String>, M: Metric + Clone>(
    registry: &mut prometheus_client::registry::Registry,
    name: N,
    help: H,
    metric: M,
) -> M {
    registry.register(name, help, metric.clone());
    metric
}

impl Metrics {
    pub fn new_with_registery(registry: &mut prometheus_client::registry::Registry) -> Self {
        Self {
            connections: register(registry, "connections", "Count of Ambient connections"),
            current_connections: register(
                registry,
                "current_connections",
                "Current count of Ambient connections",
            ),
            asset_requests: register(registry, "asset_requests", "Count of asset requests"),
            current_assets_size_bytes: register(
                registry,
                "current_assets_size_bytes",
                "Current size of assets",
            ),
            asset_response_size_bytes: register_metric(
                registry,
                "asset_response_size_bytes",
                "Size of asset responses",
                Histogram::new(
                    (0..10)
                        .into_iter()
                        .map(|i| 1024.0 * 2.0f64.powi(i))
                        .chain(
                            (0..10)
                                .into_iter()
                                .map(|i| 1024.0 * 1024.0 * 2.0f64.powi(i)),
                        )
                        .chain([1024_f64.powi(3)].into_iter()),
                ),
            ),
        }
    }

    pub fn inc_connections(&self, role: ConnectionRole) {
        tracing::trace!("Incrementing connection counter for role {:?}", role);
        self.connections
            .get_or_create(&ProxyConnectionLabels { role })
            .inc();
    }

    pub fn set_current_connections(&self, role: ConnectionRole, count: usize) {
        tracing::trace!(
            "Setting current connection count for role {:?} to {}",
            role,
            count
        );
        let Ok(count) = count.try_into() else {
            tracing::warn!("Current connection count for role {:?} is too large: {}", role, count);
            return;
        };
        self.current_connections
            .get_or_create(&ProxyConnectionLabels { role })
            .set(count);
    }

    pub fn inc_asset_requests(&self, result: AssetRequestResult) {
        tracing::trace!("Incrementing asset request counter for result {:?}", result);
        self.asset_requests
            .get_or_create(&AssetRequestLabels { result })
            .inc();
    }

    pub fn set_current_assets_size_bytes(&self, size: usize) {
        tracing::trace!("Setting current asset size to {}", size);
        let Ok(size) = size.try_into() else {
            tracing::warn!("Current asset size is too large: {}", size);
            return;
        };
        self.current_assets_size_bytes.set(size);
    }

    pub fn observe_asset_response_size_bytes(&self, size: usize) {
        tracing::trace!("Observing asset response size of {}", size);
        self.asset_response_size_bytes.observe(size as f64);
    }
}

pub fn init_subscriber<Sink>(name: String, env_filter: String, sink: Sink)
where
    Sink: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(env_filter));

    LogTracer::init().expect("Failed to set logger");

    let registry = Registry::default().with(env_filter);
    match std::env::var("LOG_FORMAT")
        .map(|v| v.to_lowercase())
        .unwrap_or("stackdriver".to_string())
        .as_str()
    {
        "bunyan" => {
            let formatting_layer = BunyanFormattingLayer::new(name, sink);
            set_global_default(registry.with(JsonStorageLayer).with(formatting_layer))
        }
        "stackdriver" => {
            set_global_default(registry.with(tracing_stackdriver::layer().with_writer(sink)))
        }
        _ => set_global_default(registry.with(tracing_subscriber::fmt::layer().with_writer(sink))),
    }
    .expect("Failed to set subscriber");
}
