use std::net::SocketAddr;

pub(crate) use clap::Parser;
use kubvernor::{start, Args};
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer, Registry};
pub enum Guard {
    Appender(WorkerGuard),
}

cfg_if::cfg_if! {
    if #[cfg(feature="envoy_xds")] {
        #[derive(Parser, Debug)]
        #[command(version, about, long_about = None)]
        pub struct CommandArgs {
            #[arg(short, long)]
            controller_name: String,
            #[arg(short, long)]
            with_opentelemetry: Option<bool>,
            #[arg(long)]
            control_plane_socket: Option<SocketAddr>,
            #[arg(long)]
            envoy_control_plane_hostname: String,
            #[arg(long)]
            envoy_control_plane_port: u16,
        }

        impl From<CommandArgs> for Args {
            fn from(value: CommandArgs) -> Self {
                let control_plane_socket = if let Some(socket) = value.control_plane_socket {
                    socket
                } else {
                    "0.0.0.0:50051".parse().expect("We expect this to work")
                };

                Args::builder()
                    .controller_name(value.controller_name)
                    .control_plane_socket(control_plane_socket)
                    .envoy_control_plane_host(value.envoy_control_plane_hostname)
                    .envoy_control_plane_port(value.envoy_control_plane_port)
                    .build()
            }
        }
    } else if #[cfg(feature = "envoy_cm")] {
        #[derive(Parser, Debug)]
        #[command(version, about, long_about = None)]
        pub struct CommandArgs {
            #[arg(short, long)]
            controller_name: String,
            #[arg(short, long)]
            with_opentelemetry: Option<bool>,
        }

        impl From<CommandArgs> for Args {
            fn from(value: CommandArgs) -> Self {
                Args::builder()
                    .controller_name(value.controller_name)
                    .control_plane_socket("0.0.0.0:50051".parse().expect("We expect this to work"))
                    .envoy_control_plane_host("localhost")
                    .envoy_control_plane_port("50051")
                    .build()
            }
        }
    } else {

    }
}

fn init_logging(args: &CommandArgs) -> Guard {
    let registry = Registry::default();
    let controller_name = args.controller_name.clone();
    let file_appender = tracing_appender::rolling::never(".", "kubvernor.log");
    let (non_blocking_appender, guard) = tracing_appender::non_blocking(file_appender);
    let file_filter = tracing_subscriber::EnvFilter::new(std::env::var("RUST_FILE_LOG").unwrap_or_else(|_| "debug".to_owned()));
    let console_filter = tracing_subscriber::EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_owned()));
    let tracing_filter = tracing_subscriber::EnvFilter::new(std::env::var("RUST_TRACE_LOG").unwrap_or_else(|_| "info".to_owned()));

    if let Some(true) = args.with_opentelemetry {
        if let Ok(exporter) = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint("http://127.0.0.1:4317")
            .with_timeout(std::time::Duration::from_secs(3))
            .build()
        {
            let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_id_generator(RandomIdGenerator::default())
                .with_sampler(Sampler::AlwaysOn)
                .with_resource(
                    opentelemetry_sdk::Resource::builder()
                        .with_attributes(vec![opentelemetry::KeyValue::new("service.name", controller_name.clone())])
                        .build(),
                )
                .build();

            let tracer = tracer_provider.tracer(controller_name);
            let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

            registry
                .with(telemetry.with_filter(tracing_filter))
                .with(fmt::layer().with_writer(non_blocking_appender).with_ansi(false).with_filter(file_filter))
                .with(fmt::layer().with_filter(console_filter))
                .init();

            Guard::Appender(guard)
        } else {
            panic!("Can't do tracing");
        }
    } else {
        registry
            .with(fmt::layer().with_writer(non_blocking_appender).with_ansi(false).with_filter(file_filter))
            .with(fmt::layer().with_filter(console_filter))
            .init();
        Guard::Appender(guard)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> kubvernor::Result<()> {
    let args = CommandArgs::parse();
    let _guard = init_logging(&args);
    let args = Args::from(args);
    start(args).await
}
