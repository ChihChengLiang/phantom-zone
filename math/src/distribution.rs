use crate::ring::{ElemFrom, ModulusOps, RingOps};
use core::{convert::identity, iter::repeat_with};
use itertools::{izip, Itertools};
use num_traits::{FromPrimitive, PrimInt, Signed};
use rand::{
    distributions::{uniform::SampleUniform, Distribution, Uniform},
    Rng, RngCore,
};
use rand_distr::Normal;

pub trait Sampler: ModulusOps {
    fn sample<T>(&self, dist: impl Distribution<T>, mut rng: impl RngCore) -> Self::Elem
    where
        Self: ElemFrom<T>,
    {
        self.elem_from(dist.sample(&mut rng))
    }

    fn sample_iter<T>(
        &self,
        dist: impl Distribution<T>,
        rng: impl RngCore,
    ) -> impl Iterator<Item = Self::Elem>
    where
        Self: ElemFrom<T>,
    {
        dist.sample_iter(rng).map(|v| self.elem_from(v))
    }

    fn sample_into<T>(
        &self,
        out: &mut [Self::Elem],
        dist: impl DistributionSized<T>,
        rng: impl RngCore,
    ) where
        Self: ElemFrom<T>,
    {
        dist.sample_map_into(out, |v| self.elem_from(v), rng)
    }

    fn sample_vec<T: Default>(
        &self,
        n: usize,
        dist: impl DistributionSized<T>,
        rng: impl RngCore,
    ) -> Vec<Self::Elem>
    where
        Self: ElemFrom<T>,
    {
        let mut out = vec![Default::default(); n];
        self.sample_into(&mut out, dist, rng);
        out
    }

    fn uniform_distribution(&self)
        -> impl Distribution<Self::Elem> + DistributionSized<Self::Elem>;

    fn sample_uniform(&self, mut rng: impl RngCore) -> Self::Elem {
        self.uniform_distribution().sample(&mut rng)
    }

    fn sample_uniform_iter(&self, rng: impl RngCore) -> impl Iterator<Item = Self::Elem> {
        self.uniform_distribution().sample_iter(rng)
    }

    fn sample_uniform_into(&self, out: &mut [Self::Elem], rng: impl RngCore) {
        self.uniform_distribution().sample_into(out, rng)
    }

    fn sample_uniform_vec(&self, n: usize, rng: impl RngCore) -> Vec<Self::Elem> {
        self.uniform_distribution().sample_vec(n, rng)
    }

    fn sample_uniform_poly(&self, rng: impl RngCore) -> Vec<Self::Elem>
    where
        Self: RingOps,
    {
        self.sample_uniform_vec(self.ring_size(), rng)
    }
}

pub trait DistributionSized<T> {
    fn sample_map_into<R: Rng, O>(self, out: &mut [O], f: impl Fn(T) -> O, rng: R);

    fn sample_into<R: Rng>(self, out: &mut [T], rng: R)
    where
        Self: Sized,
    {
        self.sample_map_into(out, identity, rng)
    }

    fn sample_vec<R: Rng>(self, n: usize, rng: R) -> Vec<T>;
}

#[derive(Clone, Copy, Debug)]
pub struct Gaussian(Normal<f64>);

impl Gaussian {
    pub fn new(std_dev: f64) -> Self {
        Self(Normal::new(0f64, std_dev).unwrap())
    }

    pub fn std_dev(&self) -> f64 {
        self.0.std_dev()
    }
}

impl<T: FromPrimitive> Distribution<T> for Gaussian {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> T {
        FromPrimitive::from_f64(self.0.sample(rng)).unwrap()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Ternary(pub usize);

impl Ternary {
    pub fn hamming_weight(self) -> usize {
        self.0
    }
}

impl<T: Signed> DistributionSized<T> for Ternary {
    fn sample_map_into<R: Rng, O>(self, out: &mut [O], f: impl Fn(T) -> O, mut rng: R) {
        assert_ne!(self.hamming_weight(), 0);
        assert!(self.hamming_weight() <= out.len());
        let indices = {
            let insert = |set: &mut [u8], idx: usize| {
                let is_none = (set[idx / 8] & 1 << (idx % 8)) == 0;
                set[idx / 8] |= 1 << (idx % 8);
                is_none
            };
            let mut set = vec![0; out.len().div_ceil(8)];
            let mut count = 0;
            for idx in Uniform::new(0, out.len()).sample_iter(&mut rng) {
                count += insert(&mut set, idx) as usize;
                if count == self.hamming_weight() {
                    break;
                }
            }
            set.into_iter().flat_map(into_bits).positions(identity)
        };
        izip!(indices, repeat_with(|| rng.next_u64()).flat_map(into_bits))
            .for_each(|(idx, bit)| out[idx] = f(if bit { T::one() } else { -T::one() }));
    }

    fn sample_vec<R: Rng>(self, n: usize, rng: R) -> Vec<T> {
        let mut out = repeat_with(T::zero).take(n).collect_vec();
        self.sample_into(&mut out, rng);
        out
    }
}

fn into_bits<T: PrimInt>(byte: T) -> impl Iterator<Item = bool> {
    (0..T::zero().count_zeros() as usize).map(move |i| (byte >> i) & T::one() == T::one())
}

macro_rules! impl_distribution_sized_by_distribution {
    ($t:ty $(where T: $bonud:ident)?) => {
        impl<T> DistributionSized<T> for $t
        where
            Self: Distribution<T>,
            $(T: $bonud)?
        {
            fn sample_map_into<R: Rng, O>(self, out: &mut [O], f: impl Fn(T) -> O, rng: R) {
                izip!(out, self.sample_iter(rng)).for_each(|(a, b)| *a = f(b));
            }

            fn sample_vec<R: Rng>(self, n: usize, rng: R) -> Vec<T>
            where
                Self: Sized,
            {
                self.sample_iter(rng).take(n).collect()
            }
        }
    };
}

impl_distribution_sized_by_distribution!(Gaussian);
impl_distribution_sized_by_distribution!(Uniform<T> where T: SampleUniform);

#[cfg(test)]
mod test {
    use crate::distribution::{DistributionSized, Ternary};
    use num_traits::Zero;
    use rand::{
        distributions::{Distribution, Uniform},
        thread_rng,
    };

    #[test]
    fn ternary() {
        let mut rng = thread_rng();
        for n in (0..12).map(|log_n| 1 << log_n) {
            for _ in 0..n.min(100) {
                let hamming_weight = Uniform::new_inclusive(1, n).sample(&mut rng);
                let out: Vec<i64> = Ternary(hamming_weight).sample_vec(n, &mut rng);
                assert_eq!(out.iter().filter(|v| !v.is_zero()).count(), hamming_weight);
            }
        }
    }
}