// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use web_time::Instant;

pub fn rate_limit_bps_to_bucket_bytes_per_sec(limit_bps: u64) -> f64 {
    if limit_bps == 0 || limit_bps >= i64::MAX as u64 {
        f64::INFINITY
    } else {
        limit_bps as f64 / 8.0
    }
}

/// The internal state of the bucket, protected by a Mutex.
struct TokenBucketInner {
    last_refill_time: Instant,
    tokens: f64,
    fill_rate: f64,
    capacity: f64,
}

/// A thread-safe TokenBucket that optimizes for the "infinite" case.
pub struct TokenBucket {
    // Fast-path flag: checked without locking
    is_infinite: AtomicBool,
    // Slow-path state: protected by a blocking Mutex (fast for simple math)
    inner: Mutex<TokenBucketInner>,
}

impl TokenBucket {
    pub fn new(capacity: f64, fill_rate: f64) -> Self {
        let sane_fill_rate = fill_rate.max(0.0);
        let sane_capacity = capacity.max(0.0);

        let infinite = !sane_fill_rate.is_finite();

        let (initial_tokens, initial_capacity) = if infinite {
            (f64::INFINITY, f64::INFINITY)
        } else {
            (sane_capacity, sane_capacity)
        };

        let inner = TokenBucketInner {
            last_refill_time: Instant::now(),
            tokens: initial_tokens,
            fill_rate: sane_fill_rate,
            capacity: initial_capacity,
        };

        TokenBucket {
            is_infinite: AtomicBool::new(infinite),
            inner: Mutex::new(inner),
        }
    }

    pub fn set_rate(&self, new_fill_rate: f64) {
        let rate = new_fill_rate.max(0.0);
        let infinite = !rate.is_finite();

        self.is_infinite.store(infinite, Ordering::Relaxed);

        let mut guard = self.inner.lock().unwrap();
        if infinite {
            guard.fill_rate = 0.0;
            guard.capacity = f64::INFINITY;
            guard.tokens = f64::INFINITY;
        } else {
            guard.fill_rate = rate;
            guard.capacity = rate;
            guard.tokens = rate;
        }
        guard.last_refill_time = Instant::now();
    }

    pub fn set_rate_preserving_tokens(&self, new_fill_rate: f64) {
        self.set_rate_with_capacity_preserving_tokens(new_fill_rate, new_fill_rate);
    }

    pub fn set_rate_with_capacity_preserving_tokens(&self, new_fill_rate: f64, new_capacity: f64) {
        let rate = new_fill_rate.max(0.0);
        let infinite = !rate.is_finite();

        self.is_infinite.store(infinite, Ordering::Relaxed);

        let mut guard = self.inner.lock().unwrap();
        guard.refill();
        if infinite {
            guard.fill_rate = 0.0;
            guard.capacity = f64::INFINITY;
            guard.tokens = f64::INFINITY;
        } else {
            guard.fill_rate = rate;
            guard.capacity = new_capacity.max(rate.min(1.0)).max(0.0);
            guard.tokens = guard.tokens.min(guard.capacity);
        }
        guard.last_refill_time = Instant::now();
    }

    #[cfg(test)]
    pub fn get_tokens(&self) -> f64 {
        self.inner.lock().unwrap().tokens
    }

    #[cfg(test)]
    pub fn get_capacity(&self) -> f64 {
        self.inner.lock().unwrap().capacity
    }

    #[cfg(test)]
    pub fn set_tokens(&self, val: f64) {
        self.inner.lock().unwrap().tokens = val;
    }

    #[cfg(test)]
    pub fn rewind_last_refill_time(&self, duration: Duration) {
        let mut guard = self.inner.lock().unwrap();
        guard.last_refill_time = guard
            .last_refill_time
            .checked_sub(duration)
            .unwrap_or_else(Instant::now);
    }

    #[cfg(test)]
    pub fn get_fill_rate(&self) -> f64 {
        self.inner.lock().unwrap().fill_rate
    }
}

impl TokenBucketInner {
    fn refill(&mut self) {
        if self.capacity.is_infinite() {
            self.tokens = f64::INFINITY;
            self.last_refill_time = Instant::now();
            return;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_refill_time);
        self.last_refill_time = now;
        if self.fill_rate > 0.0 && self.fill_rate.is_finite() {
            let tokens_to_add = elapsed.as_secs_f64() * self.fill_rate;
            self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
        }
    }
}

/// Returns immediately if the bucket is configured as infinite.
/// Otherwise, sleeps asynchronously until enough tokens are available.
pub async fn consume_tokens(bucket: &TokenBucket, amount_tokens: f64) {
    if bucket.is_infinite.load(Ordering::Relaxed) {
        return;
    }

    if amount_tokens <= 0.0 || !amount_tokens.is_finite() {
        return;
    }

    let (current_fill_rate, current_capacity) = {
        let guard = bucket.inner.lock().unwrap();
        if guard.capacity.is_infinite() {
            return;
        }
        (guard.fill_rate, guard.capacity)
    };

    if current_fill_rate <= 0.0 || !current_fill_rate.is_finite() {
        std::future::pending::<()>().await;
        return;
    }

    if amount_tokens > current_capacity {
        let required_duration = Duration::from_secs_f64(amount_tokens / current_fill_rate);
        if required_duration < Duration::from_secs(60 * 5) {
            crate::execution::time::sleep(required_duration).await;
        } else {
            tracing::warn!(
                ?required_duration,
                "Calculated sleep time for large token-bucket request exceeds limit"
            );
        }
        return;
    }

    loop {
        let wait_time = {
            let mut guard = bucket.inner.lock().unwrap();
            guard.refill();

            if guard.tokens >= amount_tokens {
                guard.tokens -= amount_tokens;
                break;
            }

            let tokens_needed = amount_tokens - guard.tokens;
            let wait_duration_secs = tokens_needed / current_fill_rate;
            Duration::from_secs_f64(wait_duration_secs.max(0.001))
        };

        crate::execution::time::sleep(wait_time).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const TOLERANCE: f64 = 1e-3;

    #[test]
    fn test_token_bucket_new() {
        let bucket = TokenBucket::new(100.0, 10.0);
        assert!((bucket.get_capacity() - 100.0).abs() < TOLERANCE);
        assert!((bucket.get_fill_rate() - 10.0).abs() < TOLERANCE);
        assert!((bucket.get_tokens() - 100.0).abs() < TOLERANCE);
    }

    #[test]
    fn test_token_bucket_new_zero_rate() {
        let bucket = TokenBucket::new(100.0, 0.0);
        assert!((bucket.get_capacity() - 100.0).abs() < TOLERANCE);
        assert!(bucket.get_fill_rate() == 0.0);
        assert!((bucket.get_tokens() - 100.0).abs() < TOLERANCE);
        assert!(!bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[test]
    fn rate_limit_zero_bps_maps_to_unlimited_bucket_rate() {
        assert!(rate_limit_bps_to_bucket_bytes_per_sec(0).is_infinite());
    }

    #[test]
    fn test_token_bucket_new_infinite_rate() {
        let bucket = TokenBucket::new(f64::INFINITY, f64::INFINITY);
        assert!(bucket.get_capacity().is_infinite());
        assert!(bucket.get_fill_rate().is_infinite());
        assert!(bucket.get_tokens().is_infinite());
        assert!(bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[test]
    fn test_token_bucket_consume_success_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        // Manual manipulation via inner lock for sync testing
        {
            let mut g = bucket.inner.lock().unwrap();
            g.refill();
            if g.tokens >= 50.0 {
                g.tokens -= 50.0;
            }
        }
        assert!((bucket.get_tokens() - 50.0).abs() < TOLERANCE);
    }

    #[tokio::test]
    async fn test_token_bucket_refill_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        bucket.set_tokens(0.0);
        assert!(bucket.get_tokens().abs() < TOLERANCE);

        bucket.rewind_last_refill_time(Duration::from_secs(2));

        {
            let mut g = bucket.inner.lock().unwrap();
            g.refill();
        }

        let tokens = bucket.get_tokens();
        assert!(
            (20.0..21.0).contains(&tokens),
            "Expected roughly 20.0 tokens after rewinding refill time, got {tokens}"
        );
    }

    #[test]
    fn test_token_bucket_set_rate_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        bucket.set_tokens(50.0);
        bucket.set_rate(200.0);
        assert!((bucket.get_fill_rate() - 200.0).abs() < TOLERANCE);
        assert!((bucket.get_capacity() - 200.0).abs() < TOLERANCE);
        assert!((bucket.get_tokens() - 200.0).abs() < TOLERANCE);
        assert!(!bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[test]
    fn test_token_bucket_set_rate_to_zero_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        bucket.set_tokens(50.0);
        bucket.set_rate(0.0);
        assert!(bucket.get_fill_rate() == 0.0);
        assert!(bucket.get_capacity() == 0.0);
        assert!(bucket.get_tokens() == 0.0);
        assert!(!bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[test]
    fn test_token_bucket_set_rate_preserving_tokens_does_not_refill_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        bucket.set_tokens(50.0);
        bucket.set_rate_preserving_tokens(200.0);
        assert!((bucket.get_fill_rate() - 200.0).abs() < TOLERANCE);
        assert!((bucket.get_capacity() - 200.0).abs() < TOLERANCE);
        assert!((bucket.get_tokens() - 50.0).abs() < TOLERANCE);
        assert!(!bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[test]
    fn test_token_bucket_set_rate_with_capacity_preserving_tokens_direct() {
        let bucket = TokenBucket::new(100.0, 10.0);
        bucket.set_tokens(80.0);
        bucket.set_rate_with_capacity_preserving_tokens(200.0, 40.0);
        assert!((bucket.get_fill_rate() - 200.0).abs() < TOLERANCE);
        assert!((bucket.get_capacity() - 40.0).abs() < TOLERANCE);
        assert!((bucket.get_tokens() - 40.0).abs() < TOLERANCE);
        assert!(!bucket.is_infinite.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_consume_tokens_unlimited_infinite_rate_direct() {
        let bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        tokio::time::timeout(
            Duration::from_millis(25),
            consume_tokens(&bucket, 1_000_000.0),
        )
        .await
        .expect("infinite token bucket should not wait for refill");
    }

    #[tokio::test]
    async fn test_consume_tokens_zero_rate_blocks_direct() {
        let bucket = Arc::new(TokenBucket::new(0.0, 0.0));
        let result =
            tokio::time::timeout(Duration::from_millis(25), consume_tokens(&bucket, 1.0)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_consume_tokens_immediate_success_direct() {
        let bucket = Arc::new(TokenBucket::new(1000.0, 100.0));
        consume_tokens(&bucket, 500.0).await;
        assert!((bucket.get_tokens() - 500.0).abs() < TOLERANCE);
    }

    #[tokio::test]
    async fn test_consume_tokens_waits_for_refill_direct() {
        let bucket = Arc::new(TokenBucket::new(1000.0, 1000.0));
        bucket.set_tokens(0.0);
        bucket.rewind_last_refill_time(Duration::from_millis(500));

        consume_tokens(&bucket, 500.0).await;
        assert!(
            bucket.get_tokens() < 1.0,
            "expected refill to be consumed, got {} tokens",
            bucket.get_tokens()
        );
    }

    #[tokio::test]
    async fn test_consume_tokens_multiple_consumers_direct() {
        // Keep refill negligible so scheduling delay between the consumers does not
        // change the depletion assertion this test is meant to exercise.
        let bucket = Arc::new(TokenBucket::new(1500.0, 1.0));

        let bucket_1 = Arc::clone(&bucket);
        let bucket_2 = Arc::clone(&bucket);

        let task_1 = tokio::spawn(async move {
            consume_tokens(&bucket_1, 500.0).await;
        });
        let task_2 = tokio::spawn(async move {
            consume_tokens(&bucket_2, 1000.0).await;
        });

        let (res1, res2) = tokio::join!(task_1, task_2);
        assert!(res1.is_ok());
        assert!(res2.is_ok());
        assert!(
            bucket.get_tokens() < 1.0,
            "expected consumers to drain available tokens, got {} tokens",
            bucket.get_tokens()
        );
    }
}
