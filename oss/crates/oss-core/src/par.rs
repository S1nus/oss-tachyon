//! A small scoped-thread map for batches of independent proof steps.

use std::{num::NonZeroUsize, sync::Mutex};

use anyhow::Result;

/// The worker count batches use: `OSS_BUILD_THREADS`, or every core.
pub fn default_threads() -> NonZeroUsize {
    std::env::var("OSS_BUILD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::thread::available_parallelism().ok())
        .unwrap_or(NonZeroUsize::MIN)
}

/// Maps `f` over `items` on up to `threads` scoped workers, keeping input
/// order. Every item runs; the results keep their own errors.
pub fn par_map_each<T, R, F>(threads: NonZeroUsize, items: Vec<T>, f: F) -> Vec<Result<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    let workers = threads.get().min(items.len());
    if workers <= 1 {
        return items.into_iter().map(f).collect();
    }
    let queue = Mutex::new(items.into_iter().enumerate());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue lock").next();
                    let Some((index, item)) = next else { break };
                    let out = f(item);
                    results.lock().expect("results lock").push((index, out));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("results lock");
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, r)| r).collect()
}

/// [`par_map_each`], failing with the first error in input order.
pub fn par_map<T, R, F>(threads: NonZeroUsize, items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    par_map_each(threads, items, f).into_iter().collect()
}
