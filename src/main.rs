use clap::Parser;
use kubvernor::{start, Args};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer, Registry};

fn init_logging() -> WorkerGuard {
    let file_appender = tracing_appender::rolling::never(".", "kubvernor.log");
    let (non_blocking_appender, guard) = tracing_appender::non_blocking(file_appender);
    let filter1 = tracing_subscriber::EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_owned()),
    );
    let filter2 = tracing_subscriber::EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_owned()),
    );

    Registry::default()
        .with(
            fmt::layer()
                .with_writer(non_blocking_appender)
                .with_ansi(false)
                .with_filter(filter1),
        )
        .with(fmt::layer().with_filter(filter2))
        .init();
    guard
}

#[tokio::main]
async fn main() -> kubvernor::Result<()> {
    let args = Args::parse();
    let _guard = init_logging();
    start(args).await
}

// async fn reconcile_crd(crd: Arc<CustomResourceDefinition>, ctx: Arc<Context>) -> Result<Action> {
//     println!("reconcile crd request: {:?}", crd.metadata.name);
//     println!("reconcile crd request status: {:?}", crd.status);
//     Ok(Action::requeue(Duration::from_secs(3600)))
// }
