use std::time::Duration;

use async_compatibility_layer::logging::shutdown_logging;
use hotshot_testing::{
    completion_task::{CompletionTaskDescription, TimeBasedCompletionTaskDescription},
    node_types::{TestTypes, WebImpl},
    overall_safety_task::OverallSafetyPropertiesDescription,
    test_builder::{TestMetadata, TimingData},
};
use tracing::instrument;

/// Web server network test
#[cfg_attr(
    async_executor_impl = "tokio",
    tokio::test(flavor = "multi_thread", worker_threads = 2)
)]
#[cfg_attr(async_executor_impl = "async-std", async_std::test)]
#[instrument]
async fn web_server_network() {
    async_compatibility_layer::logging::setup_logging();
    async_compatibility_layer::logging::setup_backtrace();
    let metadata = TestMetadata {
        timing_data: TimingData {
            round_start_delay: 25,
            next_view_timeout: 10000,
            start_delay: 120000,

            ..Default::default()
        },
        overall_safety_properties: OverallSafetyPropertiesDescription {
            num_successful_views: 35,
            ..Default::default()
        },
        completion_task_description: CompletionTaskDescription::TimeBasedCompletionTaskBuilder(
            TimeBasedCompletionTaskDescription {
                duration: Duration::from_secs(20),
            },
        ),
        ..TestMetadata::default()
    };
    metadata
        .gen_launcher::<TestTypes, WebImpl>()
        .launch()
        .run_test()
        .await;
    shutdown_logging();
}
