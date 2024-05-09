// Copyright (C) 2024 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::fmt;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use quickwit_common::metrics::GaugeGuard;
use tantivy::Executor;
use tracing::error;

static SEARCH_THREAD_POOL: OnceCell<Arc<Executor>> = OnceCell::new();

fn build_executor() -> Arc<Executor> {
    let rayon_pool = rayon::ThreadPoolBuilder::new()
        .thread_name(|thread_id| format!("quickwit-search-{thread_id}"))
        .panic_handler(|_my_panic| {
            error!("task running in the quickwit search pool panicked");
        })
        .build()
        .expect("Failed to spawn the spawning pool");
    Arc::new(Executor::ThreadPool(rayon_pool))
}

pub(crate) fn search_executor() -> Arc<Executor> {
    SEARCH_THREAD_POOL.get_or_init(build_executor).clone()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Panicked;

impl fmt::Display for Panicked {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Scheduled job panicked")
    }
}

impl std::error::Error for Panicked {}

/// Function similar to `tokio::spawn_blocking`.
///
/// Here are two important differences however:
///
/// 1) The task is running on a rayon thread pool managed by quickwit.
/// This pool is specifically used only to run CPU intensive work
/// and is configured to contain `num_cpus` cores.
///
/// 2) Before the task is effectively scheduled, we check that
/// the spawner is still interested by its result.
///
/// It is therefore required to `await` the result of this
/// function to get anywork done.
///
/// This is nice, because it makes work that has been scheduled
/// but is not running yet "cancellable".
pub async fn run_cpu_intensive<F, R>(cpu_heavy_task: F) -> Result<R, Panicked>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let span = tracing::Span::current();
    search_executor()
        .spawn_blocking(move || {
            let _guard = span.enter();
            let mut active_thread_guard =
                GaugeGuard::from_gauge(&crate::SEARCH_METRICS.active_search_threads_count);
            active_thread_guard.add(1i64);
            cpu_heavy_task()
        })
        .await
        .map_err(|_| Panicked)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_run_cpu_intensive() {
        assert_eq!(run_cpu_intensive(|| 1).await, Ok(1));
    }

    #[tokio::test]
    async fn test_run_cpu_intensive_panicks() {
        assert!(run_cpu_intensive(|| panic!("")).await.is_err());
    }

    #[tokio::test]
    async fn test_run_cpu_intensive_panicks_do_not_shrink_thread_pool() {
        for _ in 0..100 {
            assert!(run_cpu_intensive(|| panic!("")).await.is_err());
        }
    }

    #[tokio::test]
    async fn test_run_cpu_intensive_abort() {
        let counter: Arc<AtomicU64> = Default::default();
        let mut futures = Vec::new();
        for _ in 0..1_000 {
            let counter_clone = counter.clone();
            let fut = run_cpu_intensive(move || {
                std::thread::sleep(Duration::from_millis(5));
                counter_clone.fetch_add(1, Ordering::SeqCst)
            });
            // The first few num_cores tasks should run, but the other should get cancelled.
            futures.push(tokio::time::timeout(Duration::from_millis(1), fut));
        }
        futures::future::join_all(futures).await;
        assert!(counter.load(Ordering::SeqCst) < 100);
    }
}
