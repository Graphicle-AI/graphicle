// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/_minhash.py
//! Port of graphify/_minhash.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
//! Native MT19937 matches NumPy RandomState's seeded uint64 permutations.
use crate::{Result, python_compat::repr_string};
use sha1::{Digest, Sha1};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

pub const MERSENNE_PRIME: u64 = (1 << 61) - 1;
pub const MAX_HASH: u64 = 0xffff_ffff;
type Coefficients = (Vec<u64>, Vec<u64>);
type CoefficientCache = Mutex<HashMap<usize, Arc<Coefficients>>>;
type ParameterCache = Mutex<HashMap<(u64, usize), (usize, usize)>>;
static COEFFICIENTS: OnceLock<CoefficientCache> = OnceLock::new();
static PARAMETERS: OnceLock<ParameterCache> = OnceLock::new();

pub(crate) struct Mt19937 {
    state: [u32; 624],
    index: usize,
}

impl Mt19937 {
    pub(crate) fn new(seed: u32) -> Self {
        let mut state = [0; 624];
        state[0] = seed;
        for i in 1..624 {
            state[i] = 1_812_433_253u32
                .wrapping_mul(state[i - 1] ^ (state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        Self { state, index: 624 }
    }

    pub(crate) fn next_u32(&mut self) -> u32 {
        if self.index == 624 {
            for i in 0..624 {
                let y = (self.state[i] & 0x8000_0000) | (self.state[(i + 1) % 624] & 0x7fff_ffff);
                self.state[i] = self.state[(i + 397) % 624]
                    ^ (y >> 1)
                    ^ if y & 1 == 0 { 0 } else { 0x9908_b0df };
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    fn interval61(&mut self, maximum: u64) -> u64 {
        loop {
            let value =
                ((u64::from(self.next_u32()) << 32) | u64::from(self.next_u32())) & MERSENNE_PRIME;
            if value <= maximum {
                return value;
            }
        }
    }
}

pub fn coefficients(num_perm: usize) -> Arc<Coefficients> {
    // The default sketch size is asked for once per node sketch, from every
    // dedup worker: serve it without taking the shared lock.
    static DEFAULT: OnceLock<Arc<Coefficients>> = OnceLock::new();
    if num_perm == 128 {
        return DEFAULT.get_or_init(|| shared_coefficients(128)).clone();
    }
    shared_coefficients(num_perm)
}
fn shared_coefficients(num_perm: usize) -> Arc<Coefficients> {
    let mut cache = COEFFICIENTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cache
        .entry(num_perm)
        .or_insert_with(|| {
            let mut rng = Mt19937::new(1);
            // NumPy draws all a values before b, rather than alternating them.
            let a = (0..num_perm)
                .map(|_| rng.interval61(MERSENNE_PRIME - 2) + 1)
                .collect();
            let b = (0..num_perm)
                .map(|_| rng.interval61(MERSENNE_PRIME - 1))
                .collect();
            Arc::new((a, b))
        })
        .clone()
}

#[derive(Clone, Debug)]
pub struct MinHash {
    pub num_perm: usize,
    pub hashvalues: Vec<u64>,
    coefficients: Arc<Coefficients>,
}

impl Default for MinHash {
    fn default() -> Self {
        Self::new(128)
    }
}

impl MinHash {
    pub fn new(num_perm: usize) -> Self {
        Self {
            num_perm,
            hashvalues: vec![MAX_HASH; num_perm],
            coefficients: coefficients(num_perm),
        }
    }

    pub fn update(&mut self, value: &[u8]) {
        let digest = Sha1::digest(value);
        let hash = u64::from(u32::from_le_bytes(digest[..4].try_into().unwrap()));
        for (i, value) in self.hashvalues.iter_mut().enumerate() {
            // NumPy uint64 arithmetic wraps *before* the Mersenne modulus.
            let permuted = (self.coefficients.0[i]
                .wrapping_mul(hash)
                .wrapping_add(self.coefficients.1[i])
                % MERSENNE_PRIME)
                & MAX_HASH;
            *value = (*value).min(permuted);
        }
    }
}

pub fn lsh_integrate(f: impl Fn(f64) -> f64, lo: f64, hi: f64, n: usize) -> f64 {
    let h = (hi - lo) / n as f64;
    // Python 3.12 uses compensated summation for built-in sum(float).
    let (mut sum, mut correction) = (0.0f64, 0.0f64);
    for i in 0..n {
        let value = f(lo + i as f64 * h);
        let next = sum + value;
        correction += if sum.abs() >= value.abs() {
            (sum - next) + value
        } else {
            (value - next) + sum
        };
        sum = next;
    }
    h * (sum + correction)
}

pub fn optimal_lsh_params(threshold: f64, num_perm: usize) -> (usize, usize) {
    let key = (
        if threshold == 0.0 {
            0
        } else {
            threshold.to_bits()
        },
        num_perm,
    );
    let cache = PARAMETERS.get_or_init(Mutex::default);
    if let Some(value) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return *value;
    }
    let (mut best_error, mut best) = (f64::INFINITY, (1, 1));
    for bands in 1..=num_perm {
        for rows in 1..=num_perm / bands {
            let (b, r) = (bands as f64, rows as f64);
            let fp = lsh_integrate(|s| 1.0 - (1.0 - s.powf(r)).powf(b), 0.0, threshold, 128);
            let fn_ = lsh_integrate(
                |s| 1.0 - (1.0 - (1.0 - s.powf(r)).powf(b)),
                threshold,
                1.0,
                128,
            );
            let error = 0.5 * fp + 0.5 * fn_;
            if error < best_error {
                best_error = error;
                best = (bands, rows);
            }
        }
    }
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, best);
    best
}

pub struct MinHashLsh {
    pub b: usize,
    pub r: usize,
    tables: Vec<HashMap<Vec<u64>, Vec<String>>>,
    keys: BTreeSet<String>,
}

impl Default for MinHashLsh {
    fn default() -> Self {
        Self::new(0.5, 128)
    }
}

impl MinHashLsh {
    pub fn new(threshold: f64, num_perm: usize) -> Self {
        let (b, r) = optimal_lsh_params(threshold, num_perm);
        Self {
            b,
            r,
            tables: vec![HashMap::new(); b],
            keys: BTreeSet::new(),
        }
    }

    pub fn insert(&mut self, key: &str, minhash: &MinHash) -> Result<()> {
        if !self.keys.insert(key.into()) {
            return Err(format!("Key {} already exists in MinHashLSH", repr_string(key)).into());
        }
        let hv = &minhash.hashvalues;
        for (i, table) in self.tables.iter_mut().enumerate() {
            // Python slices clamp, including sketches with a different length.
            let band = hv[(i * self.r).min(hv.len())..((i + 1) * self.r).min(hv.len())].to_vec();
            table.entry(band).or_default().push(key.into());
        }
        Ok(())
    }

    pub fn query(&self, minhash: &MinHash) -> Vec<String> {
        let hv = &minhash.hashvalues;
        let mut candidates = BTreeSet::new();
        for (i, table) in self.tables.iter().enumerate() {
            let band = &hv[(i * self.r).min(hv.len())..((i + 1) * self.r).min(hv.len())];
            candidates.extend(table.get(band).into_iter().flatten().cloned());
        }
        candidates.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numpy_permutations_preserve_seed_and_draw_order() {
        let (a, b) = &*coefficients(2);
        assert_eq!(*a, [775169054918279404, 1758426461858698313]);
        assert_eq!(*b, [2109959069025161, 965365488286768773]);
    }
    #[test]
    fn lookup_uses_stable_hashes_and_rejects_duplicate_keys() {
        let mut sketch = MinHash::default();
        sketch.update(b"authentication");
        let mut lsh = MinHashLsh::default();
        lsh.insert("source-a", &sketch).unwrap();
        assert_eq!(lsh.query(&sketch), ["source-a"]);
        assert!(lsh.insert("source-a", &sketch).is_err());
    }
}
