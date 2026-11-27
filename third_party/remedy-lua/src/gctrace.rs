//! A bounded trace of the collector's work, for measuring how long the host waits on it
//! (SEC7-G1): one record a host poll (a call from the host), a collector step, or an
//! operation inside one (a resize). Off unless the host turns it on
//! (`Lua::gc_trace`); its buffer is taken whole when it is turned on, so recording never
//! allocates, and records past its end are counted, not kept. Times come from the host's
//! clock (a browser has none in `std`). Recording changes no fuel.

#[allow(unused_imports)]
use crate::nostd::prelude::*;
use crate::nostd::time::Duration;

use crate::Clock;

/// What a trace record covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TraceOp {
    /// A call from the host, start to return (its slots: all the collector scanned in it).
    Poll,
    /// A collector step that started a cycle.
    Start,
    /// A marking step (the atomic phase too, when one ran in it).
    Mark,
    /// A step that resumed a suspended atomic phase.
    Atomic,
    /// A sweeping step.
    Sweep,
    /// A table resize (its bytes: those it wrote).
    Resize,
    /// A table's new array buffer reserved (its bytes: the buffer's), a 1 to 32 MiB one's
    /// copy included.
    Reserve,
    /// A full collection's pass (`collectgarbage()`, the host's, or the emergency one an
    /// allocation at the memory limit makes): its slots, and the fuel it charged. One record
    /// a pass: one a call stopped at its fuel goes on in the next call, as a record there
    /// (SEC8-G3: at the limit, hundreds of calls spent their fuel in one, untraced). Its
    /// bytes: the phase the pass began in, `FULL_BEGAN_PAUSE`, `_MARK` (the atomic phase
    /// too) or `_SWEEP` (SEC8-F7: a pass that only sweeps scans no table slots).
    Full,
}

/// `TraceOp::Full`'s bytes: the pass began between cycles.
pub const FULL_BEGAN_PAUSE: u64 = 0;
/// `TraceOp::Full`'s bytes: the pass began while marking (the atomic phase too).
pub const FULL_BEGAN_MARK: u64 = 1;
/// `TraceOp::Full`'s bytes: the pass began while sweeping.
pub const FULL_BEGAN_SWEEP: u64 = 2;

/// One record of the collector's trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    /// The call from the host it fell in (counted from 1 since the trace started).
    pub call: u32,
    /// The collector step it fell in (counted from 1; 0: none).
    pub step: u32,
    /// The host poll it fell in (one a call now: a resumed slice is a poll of its own).
    pub poll: u32,
    /// What it covers.
    pub op: TraceOp,
    /// Table slots scanned.
    pub slots: u64,
    /// Bytes written (a resize's destination).
    pub bytes: u64,
    /// Fuel charged.
    pub fuel: u64,
    /// When it started, on the host's clock.
    pub start: Duration,
    /// When it ended.
    pub end: Duration,
}

impl TraceRecord {
    /// How long it took.
    pub fn wall(&self) -> Duration {
        self.end.saturating_sub(self.start)
    }
}

/// The trace: its clock, its buffer (`cap` records, taken at the start) and where it is.
pub(crate) struct Trace {
    clock: Clock,
    recs: Vec<TraceRecord>,
    cap: usize,
    pub dropped: u64,
    pub call: u32,
    pub poll: u32,
    pub step: u32,
}

impl Trace {
    pub(crate) fn new(clock: Clock, cap: usize) -> Trace {
        Trace { clock, recs: Vec::with_capacity(cap), cap, dropped: 0, call: 0, poll: 0, step: 0 }
    }

    pub(crate) fn now(&self) -> Duration {
        (self.clock)()
    }

    /// Records `op` from `start` to now.
    pub(crate) fn note(&mut self, op: TraceOp, step: u32, slots: u64, bytes: u64, fuel: u64, start: Duration) {
        if self.recs.len() >= self.cap {
            self.dropped += 1;
            return;
        }
        let end = self.now();
        self.recs.push(TraceRecord { call: self.call, step, poll: self.poll, op, slots, bytes, fuel, start, end });
    }

    /// The records so far (the buffer stays, empty, its room kept).
    pub(crate) fn take(&mut self) -> (Vec<TraceRecord>, u64) {
        let out = self.recs.clone();
        self.recs.clear();
        (out, crate::nostd::mem::take(&mut self.dropped))
    }
}
