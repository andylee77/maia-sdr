//! The radio's streams: what the receivers read from the control chain (its DDC's IQ, its HDL
//! demodulator's dibits), with the dibit rings' position tracking and production clock.

pub mod dibit_ring;
#[cfg(target_os = "linux")]
mod readers;

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

/// One delivery to a receiver.
#[derive(Debug)]
pub enum Input {
    /// 50 kSPS interleaved I, Q from the control DDC.
    Iq(Vec<i16>),
    /// Dibits from the control chain's HDL demodulator, four a byte (the first in the low bits).
    /// `reset`: the stream skipped (a resync or a lost delivery), so a frame in progress is lost.
    Dibits { bytes: Vec<u8>, reset: bool },
}

/// Which control-chain streams a receiver wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wants {
    pub iq: bool,
    pub dibits: bool,
}

/// Delivery counters, for the diagnostics pages.
#[derive(Debug, Default)]
pub struct StreamCounters {
    pub iq_chunks: AtomicU64,
    /// IQ chunks dropped because the receiver was behind.
    pub iq_dropped: AtomicU64,
    pub dibit_bytes: AtomicU64,
    pub dibit_resyncs: AtomicU64,
    /// Dibit deliveries lost (receiver behind, or a copy failed).
    pub dibit_lost: AtomicU64,
}

/// Starts the readers of the control chain's streams; they stop when `stop` is set or the
/// receiver hangs up.
pub trait StreamSource {
    fn control_streams(
        &self,
        wants: Wants,
        tx: SyncSender<Input>,
        stop: Arc<AtomicBool>,
        counters: Arc<StreamCounters>,
    ) -> Vec<tokio::task::JoinHandle<()>>;
}

#[cfg(not(target_os = "linux"))]
impl StreamSource for super::hw::Hardware {
    fn control_streams(
        &self,
        _: Wants,
        _: SyncSender<Input>,
        _: Arc<AtomicBool>,
        _: Arc<StreamCounters>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        Vec::new()
    }
}
