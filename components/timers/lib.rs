/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A generic timer scheduler module that can be integrated into a crossbeam based event
//! loop or used to launch a background timer thread.

#![deny(unsafe_code)]

use std::cmp::{self, Ord};
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

#[cfg(not(target_arch = "wasm32"))]
use crossbeam_channel::after;
use crossbeam_channel::{Receiver, never};
use malloc_size_of_derive::MallocSizeOf;

/// A callback to pass to the [`TimerScheduler`] to be called when the timer is
/// dispatched.
pub type BoxedTimerCallback = Box<dyn Fn() + Send + 'static>;

/// Requests a TimerEvent-Message be sent after the given duration.
#[derive(MallocSizeOf)]
pub struct TimerEventRequest {
    #[ignore_malloc_size_of = "Size of a boxed function"]
    pub callback: BoxedTimerCallback,
    pub duration: Duration,
}

impl TimerEventRequest {
    fn dispatch(self) {
        (self.callback)()
    }
}

#[derive(MallocSizeOf)]
struct ScheduledEvent {
    id: TimerId,
    request: TimerEventRequest,
    for_time: Instant,
}

#[cfg(target_arch = "wasm32")]
#[derive(MallocSizeOf)]
struct WorkerScheduledEvent {
    id: TimerId,
    request: TimerEventRequest,
    deadline_ns: u64,
}

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
fn worker_monotonic_now_ns() -> u64 {
    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        #[link_name = "worker_monotonic_now_ns"]
        fn host_now_ns() -> u64;
    }

    unsafe { host_now_ns() }
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &ScheduledEvent) -> cmp::Ordering {
        self.for_time.cmp(&other.for_time).reverse()
    }
}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &ScheduledEvent) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for ScheduledEvent {}
impl PartialEq for ScheduledEvent {
    fn eq(&self, other: &ScheduledEvent) -> bool {
        std::ptr::eq(self, other)
    }
}

#[derive(Clone, Copy, MallocSizeOf, PartialEq)]
pub struct TimerId(usize);

/// A queue of [`TimerEventRequest`]s that are stored in order of next-to-fire.
#[derive(Default, MallocSizeOf)]
pub struct TimerScheduler {
    /// A priority queue of future events, sorted by due time.
    #[cfg(not(target_arch = "wasm32"))]
    queue: BinaryHeap<ScheduledEvent>,

    #[cfg(target_arch = "wasm32")]
    worker_queue: Vec<WorkerScheduledEvent>,

    /// The current timer id, used to generate new ones.
    current_id: usize,
}

impl TimerScheduler {
    /// Schedule a new timer for on this [`TimerScheduler`].
    pub fn schedule_timer(&mut self, request: TimerEventRequest) -> TimerId {
        #[cfg(target_arch = "wasm32")]
        {
            let id = TimerId(self.current_id);
            self.current_id += 1;
            self.worker_queue.push(WorkerScheduledEvent {
                id,
                deadline_ns: worker_monotonic_now_ns()
                    .saturating_add(request.duration.as_nanos().min(u64::MAX as u128) as u64),
                request,
            });
            return id;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let for_time = Instant::now() + request.duration;

            let id = TimerId(self.current_id);
            self.current_id += 1;

            self.queue.push(ScheduledEvent {
                id,
                request,
                for_time,
            });
            id
        }
    }

    /// Cancel a timer with the given [`TimerId`]. If a timer with that id is not
    /// currently waiting to fire, do nothing.
    pub fn cancel_timer(&mut self, id: TimerId) {
        #[cfg(target_arch = "wasm32")]
        self.worker_queue.retain(|event| event.id != id);
        #[cfg(not(target_arch = "wasm32"))]
        self.queue.retain(|event| event.id != id);
    }

    /// Get a [`Receiver<Instant>`] that receives a message after waiting for the next timer
    /// to fire. If there are no timers, the channel will *never* send a message.
    pub fn wait_channel(&self) -> Receiver<Instant> {
        #[cfg(target_arch = "wasm32")]
        return never();

        #[cfg(not(target_arch = "wasm32"))]
        self.queue
            .peek()
            .map(|event| {
                let now = Instant::now();
                if event.for_time < now {
                    after(Duration::ZERO)
                } else {
                    after(event.for_time - now)
                }
            })
            .unwrap_or_else(never)
    }

    /// The deadline of the next scheduled timer, or `None` if the queue is empty.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.peek().map(|event| event.for_time)
    }

    /// Dispatch any timer events from this [`TimerScheduler`]'s `queue` when `now` is
    /// past the due time of the event.
    pub fn dispatch_completed_timers(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            let now = worker_monotonic_now_ns();
            let mut ready = Vec::new();
            let mut pending = Vec::with_capacity(self.worker_queue.len());
            for event in self.worker_queue.drain(..) {
                if event.deadline_ns <= now {
                    ready.push(event.request);
                } else {
                    pending.push(event);
                }
            }
            self.worker_queue = pending;
            for request in ready {
                request.dispatch();
            }
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let now = Instant::now();
            loop {
                match self.queue.peek() {
                    // Dispatch the event if its due time is past.
                    Some(event) if event.for_time <= now => {},
                    // Otherwise, we're done dispatching events.
                    _ => break,
                }
                // Remove the event from the priority queue (Note this only executes when the
                // first event has been dispatched
                self.queue
                    .pop()
                    .expect("Expected request")
                    .request
                    .dispatch();
            }
        }
    }

    /// Return the next Worker timer deadline as a monotonic nanosecond value.
    #[cfg(target_arch = "wasm32")]
    pub fn worker_next_deadline_ns(&self) -> Option<u64> {
        self.worker_queue
            .iter()
            .map(|event| event.deadline_ns)
            .min()
    }
}
