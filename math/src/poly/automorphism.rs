use crate::misc::as_slice::{AsMutSlice, AsSlice};
use phantom_zone_derive::AsSliceWrapper;

#[derive(Clone, Debug, AsSliceWrapper)]
pub struct AutomorphismMap<S: AsSlice<Elem = usize>> {
    #[as_slice]
    map: S,
    k: usize,
}

impl<S: AsSlice<Elem = usize>> AutomorphismMap<S> {
    pub fn ring_size(&self) -> usize {
        self.map.len()
    }

    pub fn k(&self) -> usize {
        self.k
    }

    pub fn iter(&self) -> impl Clone + Iterator<Item = (bool, usize)> + '_ {
        self.map.as_ref().iter().map(|v| {
            let sign = (v & 1) == 1;
            let idx = v >> 1;
            (sign, idx)
        })
    }

    pub fn apply<'a, T, F>(&'a self, poly: &'a [T], neg: F) -> impl 'a + Clone + Iterator<Item = T>
    where
        T: Copy,
        F: 'a + Clone + Fn(&T) -> T,
    {
        debug_assert_eq!(self.map.len(), poly.len());
        self.iter()
            .map(move |(sign, idx)| if sign { neg(&poly[idx]) } else { poly[idx] })
    }
}

impl AutomorphismMapOwned {
    pub fn new(ring_size: usize, k: i64) -> Self {
        debug_assert!(ring_size.is_power_of_two());
        let mask = ring_size - 1;
        let log_n = ring_size.ilog2();
        let k = k.rem_euclid(2 * ring_size as i64) as usize;
        let mut map = vec![0; ring_size];
        (0..ring_size).for_each(|i| {
            let j = i * k;
            let sign = (j >> log_n) & 1;
            map[j & mask] = (i << 1) | sign
        });
        Self { map, k }
    }
}

#[cfg(test)]
mod test {
    use crate::{modulus::powers_mod, poly::automorphism::AutomorphismMap};
    use core::ops::Neg;
    use itertools::Itertools;

    fn automorphism<T: Copy + Default + Neg<Output = T>>(input: &[T], k: i64) -> Vec<T> {
        assert!(input.len().is_power_of_two());
        let n = input.len();
        let k = k.rem_euclid(2 * n as i64) as usize;
        let mut out = vec![T::default(); n];
        (0..n)
            .map(|i| (i, (i * k) % (2 * n)))
            .for_each(|(i, j)| out[j % n] = if j < n { input[i] } else { -input[i] });
        out
    }

    #[test]
    fn automorphism_iter() {
        for log_n in 0..10 {
            let n = 1 << log_n;
            let indices = (0..n as i64).collect_vec();
            for k in powers_mod(5, 2 * n as u64).take(n / 2) {
                let auto_map = AutomorphismMap::new(n, k as _);
                assert_eq!(
                    auto_map.apply(&indices, |i| -i).collect_vec(),
                    automorphism(&indices, k as _)
                );
            }
        }
    }
}
