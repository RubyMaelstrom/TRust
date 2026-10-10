//! A single-producer, single-consumer ring of interleaved samples between
//! a context's rendering thread and its audio device's thread. The device
//! side never blocks or allocates.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub struct Ring {
    samples: Box<[AtomicU32]>,
    /// Total samples written and read (never wrap in practice).
    written: AtomicU64,
    read: AtomicU64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: (0..capacity.max(1)).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// Samples waiting to be read.
    pub fn len(&self) -> usize {
        (self.written.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)) as usize
    }

    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Producer: append `data`, which must fit.
    pub fn push(&self, data: &[f32]) {
        debug_assert!(data.len() <= self.free());
        let start = self.written.load(Ordering::Relaxed);
        let capacity = self.samples.len() as u64;
        for (offset, sample) in data.iter().enumerate() {
            self.samples[((start + offset as u64) % capacity) as usize]
                .store(sample.to_bits(), Ordering::Relaxed);
        }
        self.written
            .store(start + data.len() as u64, Ordering::Release);
    }

    /// Consumer: fill `out` from the ring; returns how many samples were
    /// available (the rest of `out` is untouched).
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let start = self.read.load(Ordering::Relaxed);
        let available = (self.written.load(Ordering::Acquire) - start) as usize;
        let count = available.min(out.len());
        let capacity = self.samples.len() as u64;
        for (offset, sample) in out[..count].iter_mut().enumerate() {
            *sample = f32::from_bits(
                self.samples[((start + offset as u64) % capacity) as usize].load(Ordering::Relaxed),
            );
        }
        self.read.store(start + count as u64, Ordering::Release);
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_reports_space() {
        let ring = Ring::new(5);
        ring.push(&[1.0, 2.0, 3.0]);
        let mut out = [0.0; 2];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out, [1.0, 2.0]);
        ring.push(&[4.0, 5.0, 6.0, 7.0]);
        assert_eq!(ring.free(), 0);
        let mut out = [0.0; 8];
        assert_eq!(ring.pop(&mut out), 5);
        assert_eq!(out[..5], [3.0, 4.0, 5.0, 6.0, 7.0]);
    }
}
