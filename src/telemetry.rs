use tracing::subscriber::set_global_default;
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};

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
