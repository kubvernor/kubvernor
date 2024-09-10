use clap::Parser;
use kubvernor::{start, Args};

#[tokio::main]
async fn main() -> kubvernor::Result<()> {
    let args = Args::parse();
    env_logger::init();
    start(args).await
}

// async fn reconcile_crd(crd: Arc<CustomResourceDefinition>, ctx: Arc<Context>) -> Result<Action> {
//     println!("reconcile crd request: {:?}", crd.metadata.name);
//     println!("reconcile crd request status: {:?}", crd.status);
//     Ok(Action::requeue(Duration::from_secs(3600)))
// }
