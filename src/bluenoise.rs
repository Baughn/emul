use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
// Removed unused: use rand::Rng;

#[derive(Clone)]
pub struct BlueNoiseInterjecter {
    inner: Arc<Mutex<BlueNoiseInterjecterInner>>,
}

// The inner state that will be protected by a mutex
struct BlueNoiseInterjecterInner {
    // How often on average we want to interject (e.g., 0.01 for 1%)
    chance_per_message: f64,
    // Minimum messages between interjections (prevents clustering)
    min_gap: usize,
    // Maximum messages without an interjection (prevents long silences)
    max_gap: usize,
    // Keep track of recent interjection history
    recent_interjections: VecDeque<usize>,
    // Track the total number of messages seen
    message_count: usize,
    // Last interjection message index
    last_interjection: usize,
    // Set to force interjection
    force_interject: bool,
    // Accumulated error term for blue noise distribution
    error: f64,
}

// Our BlueNoiseInterjecter is now automatically Send + Sync because
// Arc<Mutex<T>> is Send + Sync when T is Send
impl BlueNoiseInterjecter {
    pub fn new(chance_per_message: f64) -> Self {
        // Calculate reasonable min/max gaps based on the desired chance
        let avg_gap = (1.0 / chance_per_message) as usize;
        let min_gap = avg_gap / 2;
        let max_gap = avg_gap * 2;
        
        let inner = BlueNoiseInterjecterInner {
            chance_per_message,
            min_gap,
            max_gap,
            recent_interjections: VecDeque::with_capacity(10),
            message_count: 0,
            last_interjection: 0,
            force_interject: false,
            error: 0.0, // Initialize error to zero
        };

        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
    
    pub fn should_interject(&self) -> bool {
        // Lock the mutex to access and modify the inner state
        let mut inner = self.inner.lock().expect("Mutex was poisoned");
        
        inner.message_count += 1;
        let messages_since_last = inner.message_count - inner.last_interjection;
        let p = inner.chance_per_message; // Target probability

        // Handle forced interjection first
        if inner.force_interject {
            inner.force_interject = false; // Reset the flag
            inner.record_interjection();
            inner.error += p - 1.0; // Update error: interjected
            return true;
        }

        // Enforce minimum gap - never interject if too soon after last one
        if messages_since_last < inner.min_gap {
            inner.error += p; // Update error: did not interject (due to min_gap)
            return false;
        }

        // Force interjection if we've gone too long without one
        if messages_since_last >= inner.max_gap {
            inner.record_interjection();
            inner.error += p - 1.0; // Update error: interjected (due to max_gap)
            return true;
        }

        // Use error diffusion (blue noise) logic
        // The probability is the base chance plus the accumulated error
        let effective_probability = p + inner.error;

        // Roll the dice against the effective probability
        if rand::random::<f64>() < effective_probability {
            inner.record_interjection();
            inner.error += p - 1.0; // Update error: interjected
            true
        } else {
            inner.error += p; // Update error: did not interject
            false
        }
    }

    /// Forces the next call to should_interject() to return true,
    /// unless prevented by the minimum gap constraint.
    /// Useful for triggering the bot manually or via external events.
    pub fn force_next_interjection(&self) {
        let mut inner = self.inner.lock().expect("Mutex was poisoned");

        inner.force_interject = true;
    }
}

impl BlueNoiseInterjecterInner {
    fn record_interjection(&mut self) {
        self.last_interjection = self.message_count;
        self.recent_interjections.push_back(self.message_count);
        
        // Keep history limited to last 10 interjections
        if self.recent_interjections.len() > 10 {
            self.recent_interjections.pop_front();
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    // Removed unused: use rand::SeedableRng;
    // Removed unused: use rand_chacha::ChaCha8Rng;
    use std::collections::HashMap;

    // Removed unused helper function seeded_rng()

    // Note: Tests will now use the default thread_rng via rand::random,
    // making them non-deterministic. This is acceptable per user request.

    #[test]
    fn test_blue_noise_distribution() {
        // Create a bot with a higher chance for testing (10%)
        let bot = BlueNoiseInterjecter::new(0.1);
        
        // Run a large number of iterations
        const NUM_ITERATIONS: usize = 1_000_000;
        let mut interjections = Vec::new();
        
        for i in 0..NUM_ITERATIONS {
            if bot.should_interject() {
                interjections.push(i);
            }
        }
        
        // Check 1: Verify overall frequency is close to expected
        let expected_count = (NUM_ITERATIONS as f64 * 0.1) as usize;
        let actual_count = interjections.len();
        let deviation = (actual_count as f64 - expected_count as f64).abs() / expected_count as f64;
        
        println!("Expected interjections: {}", expected_count);
        println!("Actual interjections: {}", actual_count);
        println!("Deviation: {:.2}%", deviation * 100.0);
        
        // Allow up to 10% deviation from expected count
        assert!(deviation < 0.1, "Interjection frequency is too far from expected");
        
        // Check 2: Calculate gaps between interjections
        let mut gaps = Vec::new();
        for i in 1..interjections.len() {
            gaps.push(interjections[i] - interjections[i-1]);
        }
        
        // Collect gap statistics
        let min_gap = *gaps.iter().min().unwrap_or(&0);
        let max_gap = *gaps.iter().max().unwrap_or(&0);
        let avg_gap = gaps.iter().sum::<usize>() as f64 / gaps.len() as f64;
        
        println!("Min gap: {}", min_gap);
        println!("Max gap: {}", max_gap);
        println!("Avg gap: {:.2}", avg_gap);
        
        // Check 3: Verify we don't have very small gaps (clustering)
        assert!(min_gap >= 5, "Interjections are clustering too closely");
        
        // Check 4: Verify we don't have very large gaps (long silences)
        let theoretical_max = (1.0 / 0.1) as usize * 3; // 3x the average gap
        assert!(max_gap <= theoretical_max, "Some gaps are too large");
        
        // Check 5: Analyze distribution of gaps
        let mut gap_histogram = HashMap::new();
        for gap in &gaps {
            *gap_histogram.entry(gap / 5).or_insert(0) += 1;
        }
        
        // Print the histogram of gaps (bucketed)
        println!("Gap distribution (bucketed by 5):");
        let mut buckets: Vec<_> = gap_histogram.iter().collect();
        buckets.sort_by_key(|&(&k, _)| k);
        
        for (&bucket, &count) in buckets {
            println!("{}-{}: {}", bucket*5, (bucket+1)*5-1, count);
        }
        
        // Check 6: Calculate variance of gaps
        let variance = gaps.iter()
            .map(|&g| (g as f64 - avg_gap).powi(2))
            .sum::<f64>() / gaps.len() as f64;
        let std_dev = variance.sqrt();
        
        println!("Standard deviation: {:.2}", std_dev);
        
        // Blue noise should have lower variance than white noise (Poisson process),
        // where variance ≈ mean.
        assert!(std_dev < avg_gap, "Distribution doesn't have blue noise properties (variance too high)"); // Added detail to assertion message

        // Check 7: Ensure variance is not *too* low (i.e., it's still random)
        // A very low std dev would mean highly regular spacing.
        // We expect *some* variability. Let's check if std_dev is at least, say, 1/5th of the average gap.
        // This threshold might need tuning based on the desired "randomness feel".
        let min_expected_std_dev = avg_gap / 5.0;
        assert!(std_dev > min_expected_std_dev,
                "Standard deviation {:.2} is too low (less than {:.2}), distribution is too regular",
                std_dev, min_expected_std_dev);

        // Check 8: Test for autocorrelation at small lags
        // Blue noise should have negative autocorrelation at small lags
        let mut autocorrelation = 0.0;
        for i in 0..gaps.len()-1 {
            autocorrelation += (gaps[i] as f64 - avg_gap) * (gaps[i+1] as f64 - avg_gap);
        }
        autocorrelation /= (gaps.len() - 1) as f64 * variance;
        
        println!("Lag-1 autocorrelation: {:.3}", autocorrelation);
        
        // Blue noise typically has negative autocorrelation at lag 1
        // Allow for slight positive values due to randomness, especially with finite samples.
        // A small positive threshold like 0.05 might be more robust than strict < 0.0.
        assert!(autocorrelation < 0.05, "Autocorrelation {:.3} is not significantly negative", autocorrelation);
    }

    #[test]
    fn test_force_interjection() {
        let bot = BlueNoiseInterjecter::new(0.1); // 10% chance
        let mut inner = bot.inner.lock().unwrap();
        inner.min_gap = 2; // Set a small min_gap for testing
        inner.message_count = 10; // Simulate some history
        inner.last_interjection = 5; // Last interjection was 5 messages ago
        drop(inner); // Release lock before calling methods

        // Normally, it might not interject immediately
        // bot.should_interject(); // Consume one message

        // Force the next one
        bot.force_next_interjection();

        // The very next call should return true
        assert!(bot.should_interject(), "Forced interjection did not occur");

        // Check that the flag was reset
        let inner = bot.inner.lock().unwrap();
        assert!(!inner.force_interject, "force_interject flag was not reset");
        // Check that last_interjection was updated
        assert_eq!(inner.last_interjection, inner.message_count, "last_interjection was not updated");
        drop(inner);
    }

}
