//! Core algorithms.

/// CRC-32 checksum of a byte slice (IEEE polynomial), used to detect corrupted downloads.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Sort numbers by repeatedly splitting the list in halves and combining sorted halves.
pub fn merge_sort(values: &mut Vec<i64>) {
    if values.len() <= 1 {
        return;
    }
    let mut right = values.split_off(values.len() / 2);
    merge_sort(values);
    merge_sort(&mut right);
    let mut merged = Vec::with_capacity(values.len() + right.len());
    let (mut i, mut j) = (0, 0);
    while i < values.len() && j < right.len() {
        if values[i] <= right[j] {
            merged.push(values[i]);
            i += 1;
        } else {
            merged.push(right[j]);
            j += 1;
        }
    }
    merged.extend_from_slice(&values[i..]);
    merged.extend_from_slice(&right[j..]);
    *values = merged;
}

/// Index of a target in an ascending slice, halving the search range each step.
pub fn binary_search(sorted: &[i64], target: i64) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, sorted.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match sorted[mid].cmp(&target) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// Greatest common divisor of two integers with Euclid's algorithm.
pub fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Whether a year has 366 days in the Gregorian calendar.
pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// A growable ring of fixed capacity that overwrites the oldest element when full.
pub struct RingBuffer<T> {
    items: Vec<Option<T>>,
    head: usize,
}

impl<T> RingBuffer<T> {
    /// Append an element, replacing the oldest one when the ring is full.
    pub fn push(&mut self, item: T) {
        let slot = self.head % self.items.len();
        self.items[slot] = Some(item);
        self.head += 1;
    }
}
