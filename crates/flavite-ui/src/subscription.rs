//! Iced subscription timing for lossy pulses, lossless notices, and operations.

use iced::{Subscription, time};
use std::time::Duration;

/// UI pump cadence. Work itself stays on the core worker threads.
pub const PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Tick emitted for the application's regular host pump.
#[derive(Debug, Clone, Copy)]
pub struct PumpTick;

/// Create a periodic host-pump subscription.
pub fn pump() -> Subscription<PumpTick> {
    time::every(PUMP_INTERVAL).map(|_| PumpTick)
}
