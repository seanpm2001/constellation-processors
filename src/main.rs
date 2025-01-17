mod aggregator;
mod epoch;
mod lake;
mod lakesink;
mod models;
mod profiler;
mod prometheus;
mod record_stream;
mod schema;
mod server;
mod star;
mod util;

use aggregator::start_aggregation;
use chrono::Duration;
use clap::{ArgGroup, Parser};
use dotenvy::dotenv;
use env_logger::Env;
use epoch::CurrentEpochInfo;
use epoch::EpochConfig;
use futures::future::try_join_all;
use lakesink::start_lakesink;
use prometheus::{create_metric_server, DataLakeMetrics};
use prometheus_client::registry::Registry;
use server::start_server;
use std::env;
use std::process;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use jemallocator::Jemalloc;
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[macro_use]
extern crate log;

#[macro_use]
extern crate diesel;

const SENTRY_DSN_ENV_KEY: &str = "SENTRY_DSN";

#[derive(Parser, Debug, Clone)]
#[clap(version, about)]
#[clap(group(
    ArgGroup::new("process-mode")
      .required(true)
      .multiple(true)
      .args(&["aggregator", "lake_sink", "server"])
))]
struct CliArgs {
  #[clap(short, long, help = "Enable server mode")]
  server: bool,

  #[clap(short, long, help = "Enable lake sink mode")]
  lake_sink: bool,

  #[clap(short, long, help = "Enable aggregator mode")]
  aggregator: bool,

  #[clap(
    long,
    help = "Output aggregated measurements to stdout instead of Kafka"
  )]
  output_measurements_to_stdout: bool,

  #[clap(long, default_value = "16", help = "Worker task count for aggregator")]
  agg_worker_count: usize,

  #[clap(
    long,
    default_value = "650000",
    help = "Max messages to consume per aggregator iteration"
  )]
  agg_msg_collect_count: usize,

  #[clap(long, default_value = "604800", help = "Epoch length in seconds")]
  epoch_length_secs: usize,

  #[clap(
    long,
    default_value = "wos",
    help = "Epoch date field to use when storing results"
  )]
  epoch_date_field_name: String,

  #[clap(long, default_value = "3", help = "Max iterations for aggregator")]
  agg_iterations: usize,

  #[clap(long, default_value = "16", help = "Worker task count for server")]
  server_worker_count: usize,

  #[clap(long, default_value = "2", help = "Kafka consumer count for lake sink")]
  lakesink_consumer_count: usize,

  #[clap(long, help = "Current epoch value to use for testing purposes")]
  test_epoch: Option<u8>,
}

#[tokio::main]
async fn main() {
  let cli_args = CliArgs::parse();

  dotenv().ok();
  env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

  let _sentry_guard = env::var(SENTRY_DSN_ENV_KEY)
    .ok()
    .map(|dsn| sentry::init(dsn));

  let mut dl_tasks = Vec::new();
  let mut dl_metrics_server: Option<JoinHandle<_>> = None;

  let mut lakesink_cancel_tokens = Vec::new();
  if cli_args.lake_sink {
    let mut registry = <Registry>::default();
    let dl_metrics = Arc::new(DataLakeMetrics::default());
    dl_metrics.register_metrics(&mut registry);

    dl_metrics_server = Some(tokio::spawn(create_metric_server(registry, 9089).unwrap()));

    for _ in 0..cli_args.lakesink_consumer_count {
      let dl_metrics = dl_metrics.clone();

      let cancel_token = CancellationToken::new();
      let cloned_token = cancel_token.clone();
      dl_tasks.push(tokio::spawn(async move {
        let res = start_lakesink(
          dl_metrics,
          cloned_token.clone(),
          cli_args.output_measurements_to_stdout,
        )
        .await;
        if let Err(e) = res {
          error!("Lake sink task failed: {:?}", e);
          process::exit(1);
        }
      }));
      lakesink_cancel_tokens.push(cancel_token);
    }
  }

  if cli_args.aggregator {
    let epoch_length = Duration::seconds(cli_args.epoch_length_secs as i64);
    let current_epoch = if let Some(epoch) = cli_args.test_epoch {
      CurrentEpochInfo::test_info(epoch, epoch_length)
    } else {
      CurrentEpochInfo::retrieve().await
    };
    let epoch_config = Arc::new(EpochConfig {
      current_epoch,
      epoch_length,
      epoch_date_field_name: cli_args.epoch_date_field_name,
    });
    start_aggregation(
      cli_args.agg_worker_count,
      cli_args.agg_msg_collect_count,
      cli_args.agg_iterations,
      cli_args.output_measurements_to_stdout,
      epoch_config,
    )
    .await
    .unwrap();
    if cli_args.lake_sink {
      dl_metrics_server.unwrap().await.unwrap().unwrap();
      lakesink_cancel_tokens.iter().for_each(|t| t.cancel());
      try_join_all(dl_tasks).await.unwrap();
    }
    return;
  }

  if cli_args.server {
    start_server(cli_args.server_worker_count).await.unwrap();
  } else if cli_args.lake_sink {
    dl_metrics_server.unwrap().await.unwrap().unwrap();
    lakesink_cancel_tokens.iter().for_each(|t| t.cancel());
    try_join_all(dl_tasks).await.unwrap();
  }
}
