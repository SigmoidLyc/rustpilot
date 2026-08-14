use std::future::Future;

use futures_util::{stream::FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionMode {
    ParallelSafe,
    Exclusive,
}

pub(crate) const MAX_PARALLEL_TOOL_CALLS: usize = 4;

#[derive(Debug)]
pub(crate) struct ParallelPoolResult<T> {
    pub(crate) next_index: usize,
    pub(crate) results: Vec<(usize, T)>,
    pub(crate) aborted: bool,
}

pub(crate) fn execution_mode(name: &str, arguments: &Value) -> ExecutionMode {
    match name {
        "rust_clock"
        | "rust_web_search"
        | "rust_crawl4ai"
        | "rust_data_analysis"
        | "rust_sandbox_vision" => ExecutionMode::ParallelSafe,
        "rust_http"
            if arguments
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .eq_ignore_ascii_case("GET") =>
        {
            ExecutionMode::ParallelSafe
        }
        "rust_files" | "rust_sandbox_files" => match arguments
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "list" | "read" | "exists" => ExecutionMode::ParallelSafe,
            _ => ExecutionMode::Exclusive,
        },
        "rust_code" => match arguments
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "read" | "list" | "glob" | "grep" | "status" | "diff" => ExecutionMode::ParallelSafe,
            _ => ExecutionMode::Exclusive,
        },
        _ => ExecutionMode::Exclusive,
    }
}

pub(crate) async fn run_parallel_pool<C, F, Fut, T>(
    start: usize,
    end: usize,
    max_parallel: usize,
    cancel: &CancellationToken,
    mut classify: C,
    execute: F,
) -> ParallelPoolResult<T>
where
    C: FnMut(usize) -> ExecutionMode,
    F: Fn(usize) -> Fut,
    Fut: Future<Output = T>,
{
    let max_parallel = max_parallel.max(1);
    let mut next_index = start;
    let mut aborted = cancel.is_cancelled();
    let mut in_flight = FuturesUnordered::new();
    let mut results = Vec::new();

    loop {
        while !aborted && next_index < end && in_flight.len() < max_parallel {
            if classify(next_index) != ExecutionMode::ParallelSafe {
                break;
            }
            let index = next_index;
            let future = execute(index);
            in_flight.push(async move { (index, future.await) });
            next_index += 1;
            if cancel.is_cancelled() {
                aborted = true;
            }
        }

        let Some((index, result)) = in_flight.next().await else {
            break;
        };
        results.push((index, result));
        if cancel.is_cancelled() {
            aborted = true;
        }
    }

    results.sort_unstable_by_key(|(index, _)| *index);
    ParallelPoolResult {
        next_index,
        results,
        aborted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::time::{sleep, Duration};

    #[test]
    fn only_read_operations_are_parallel_safe() {
        assert_eq!(
            execution_mode("rust_files", &json!({"operation":"read"})),
            ExecutionMode::ParallelSafe
        );
        assert_eq!(
            execution_mode("rust_files", &json!({"operation":"write"})),
            ExecutionMode::Exclusive
        );
        assert_eq!(
            execution_mode("rust_http", &json!({"method":"POST"})),
            ExecutionMode::Exclusive
        );
        assert_eq!(
            execution_mode("rust_code", &json!({"operation":"check"})),
            ExecutionMode::Exclusive
        );
    }

    #[tokio::test]
    async fn rolling_pool_replenishes_a_slot_before_the_slowest_sibling_finishes() {
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let release_first = Arc::new(tokio::sync::Notify::new());
        let release_second = Arc::new(tokio::sync::Notify::new());
        let release_third = Arc::new(tokio::sync::Notify::new());
        let pool = tokio::spawn({
            let started = Arc::clone(&started);
            let release_first = Arc::clone(&release_first);
            let release_second = Arc::clone(&release_second);
            let release_third = Arc::clone(&release_third);
            async move {
                run_parallel_pool(
                    0,
                    4,
                    2,
                    &CancellationToken::new(),
                    |_| ExecutionMode::ParallelSafe,
                    move |index| {
                        let started = Arc::clone(&started);
                        let release_first = Arc::clone(&release_first);
                        let release_second = Arc::clone(&release_second);
                        let release_third = Arc::clone(&release_third);
                        async move {
                            started.lock().expect("start log should lock").push(index);
                            match index {
                                0 => release_first.notified().await,
                                1 => release_second.notified().await,
                                2 => release_third.notified().await,
                                _ => {}
                            }
                            index
                        }
                    },
                )
                .await
            }
        });

        for _ in 0..100 {
            if started.lock().expect("start log should lock").len() == 2 {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(&*started.lock().expect("start log should lock"), &[0, 1]);
        release_first.notify_one();
        for _ in 0..100 {
            if started.lock().expect("start log should lock").len() == 3 {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(&*started.lock().expect("start log should lock"), &[0, 1, 2]);
        release_second.notify_one();
        release_third.notify_one();
        let result = pool.await.expect("pool should settle");
        assert!(!result.aborted);
        assert_eq!(result.next_index, 4);
        assert_eq!(result.results, vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[tokio::test]
    async fn cancellation_stops_replenishment_and_drains_started_calls() {
        let cancel = CancellationToken::new();
        let started = Arc::new(AtomicUsize::new(0));
        let pool = tokio::spawn({
            let cancel = cancel.clone();
            let cancel_for_closure = cancel.clone();
            let started = Arc::clone(&started);
            async move {
                run_parallel_pool(
                    0,
                    8,
                    2,
                    &cancel,
                    |_| ExecutionMode::ParallelSafe,
                    move |index| {
                        let cancel = cancel_for_closure.clone();
                        let started = Arc::clone(&started);
                        async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            tokio::select! {
                                _ = cancel.cancelled() => index,
                                _ = sleep(Duration::from_secs(60)) => index,
                            }
                        }
                    },
                )
                .await
            }
        });
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) == 2 {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        cancel.cancel();
        let result = pool.await.expect("pool should settle");
        assert!(result.aborted);
        assert_eq!(result.next_index, 2);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(result.results.len(), 2);
    }
}
