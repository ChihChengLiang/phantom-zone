use std::fmt::Debug;

use itertools::{izip, Itertools};
use num_traits::{abs, Zero};

use crate::{
    backend::{ArithmeticOps, VectorOps},
    decomposer::Decomposer,
    lwe,
    num::UnsignedInteger,
    random::{DefaultSecureRng, RandomGaussianDist, RandomUniformDist, DEFAULT_RNG},
    utils::{fill_random_ternary_secret_with_hamming_weight, TryConvertFrom, WithLocal},
    Matrix, MatrixEntity, MatrixMut, Row, RowMut, Secret,
};

trait LweKeySwitchParameters {
    fn n_in(&self) -> usize;
    fn n_out(&self) -> usize;
    fn d_ks(&self) -> usize;
}

trait LweCiphertext<M: Matrix> {}

pub struct LweSecret {
    values: Vec<i32>,
}

impl Secret for LweSecret {
    type Element = i32;
    fn values(&self) -> &[Self::Element] {
        &self.values
    }
}

impl LweSecret {
    pub(crate) fn random(hw: usize, n: usize) -> LweSecret {
        DefaultSecureRng::with_local_mut(|rng| {
            let mut out = vec![0i32; n];
            fill_random_ternary_secret_with_hamming_weight(&mut out, hw, rng);

            LweSecret { values: out }
        })
    }
}

pub(crate) fn lwe_key_switch<
    M: Matrix,
    Ro: AsMut<[M::MatElement]> + AsRef<[M::MatElement]>,
    Op: VectorOps<Element = M::MatElement> + ArithmeticOps<Element = M::MatElement>,
    D: Decomposer<Element = M::MatElement>,
>(
    lwe_out: &mut Ro,
    lwe_in: &Ro,
    lwe_ksk: &M,
    operator: &Op,
    decomposer: &D,
) {
    assert!(lwe_ksk.dimension().0 == ((lwe_in.as_ref().len() - 1) * decomposer.d()));
    assert!(lwe_out.as_ref().len() == lwe_ksk.dimension().1);

    let lwe_in_a_decomposed = lwe_in
        .as_ref()
        .iter()
        .skip(1)
        .flat_map(|ai| decomposer.decompose(ai));
    izip!(lwe_in_a_decomposed, lwe_ksk.iter_rows()).for_each(|(ai_j, beta_ij_lwe)| {
        operator.elwise_fma_scalar_mut(lwe_out.as_mut(), beta_ij_lwe.as_ref(), &ai_j);
    });

    let out_b = operator.add(&lwe_out.as_ref()[0], &lwe_in.as_ref()[0]);
    lwe_out.as_mut()[0] = out_b;
}

pub fn lwe_ksk_keygen<
    Mmut: MatrixMut,
    S,
    Op: VectorOps<Element = Mmut::MatElement> + ArithmeticOps<Element = Mmut::MatElement>,
    R: RandomGaussianDist<Mmut::MatElement, Parameters = Mmut::MatElement>
        + RandomUniformDist<[Mmut::MatElement], Parameters = Mmut::MatElement>,
>(
    from_lwe_sk: &[S],
    to_lwe_sk: &[S],
    ksk_out: &mut Mmut,
    gadget: &[Mmut::MatElement],
    operator: &Op,
    rng: &mut R,
) where
    <Mmut as Matrix>::R: RowMut,
    Mmut::R: TryConvertFrom<[S], Parameters = Mmut::MatElement>,
    Mmut::MatElement: Zero + Debug,
{
    assert!(ksk_out.dimension() == (from_lwe_sk.len() * gadget.len(), to_lwe_sk.len() + 1,));

    let d = gadget.len();

    let modulus = VectorOps::modulus(operator);
    let mut neg_sk_in_m = Mmut::R::try_convert_from(from_lwe_sk, &modulus);
    operator.elwise_neg_mut(neg_sk_in_m.as_mut());
    let sk_out_m = Mmut::R::try_convert_from(to_lwe_sk, &modulus);

    izip!(
        neg_sk_in_m.as_ref(),
        ksk_out.iter_rows_mut().chunks(d).into_iter()
    )
    .for_each(|(neg_sk_in_si, d_ks_lwes)| {
        izip!(gadget.iter(), d_ks_lwes.into_iter()).for_each(|(f, lwe)| {
            // sample `a`
            RandomUniformDist::random_fill(rng, &modulus, &mut lwe.as_mut()[1..]);

            // a * z
            let mut az = Mmut::MatElement::zero();
            izip!(lwe.as_ref()[1..].iter(), sk_out_m.as_ref()).for_each(|(ai, si)| {
                let ai_si = operator.mul(ai, si);
                az = operator.add(&az, &ai_si);
            });

            // a*z + (-s_i)*\beta^j + e
            let mut b = operator.add(&az, &operator.mul(f, neg_sk_in_si));
            let mut e = Mmut::MatElement::zero();
            RandomGaussianDist::random_fill(rng, &modulus, &mut e);
            b = operator.add(&b, &e);

            lwe.as_mut()[0] = b;

            // dbg!(&lwe.as_mut(), &f);
        })
    });
}

/// Encrypts encoded message m as LWE ciphertext
pub fn encrypt_lwe<
    Ro: Row + RowMut,
    R: RandomGaussianDist<Ro::Element, Parameters = Ro::Element>
        + RandomUniformDist<[Ro::Element], Parameters = Ro::Element>,
    S,
    Op: ArithmeticOps<Element = Ro::Element>,
>(
    lwe_out: &mut Ro,
    m: &Ro::Element,
    s: &[S],
    operator: &Op,
    rng: &mut R,
) where
    Ro: TryConvertFrom<[S], Parameters = Ro::Element>,
    Ro::Element: Zero,
{
    let s = Ro::try_convert_from(s, &operator.modulus());
    assert!(s.as_ref().len() == (lwe_out.as_ref().len() - 1));

    // a*s
    RandomUniformDist::random_fill(rng, &operator.modulus(), &mut lwe_out.as_mut()[1..]);
    let mut sa = Ro::Element::zero();
    izip!(lwe_out.as_mut().iter().skip(1), s.as_ref()).for_each(|(ai, si)| {
        let tmp = operator.mul(ai, si);
        sa = operator.add(&tmp, &sa);
    });

    // b = a*s + e + m
    let mut e = Ro::Element::zero();
    RandomGaussianDist::random_fill(rng, &operator.modulus(), &mut e);
    let b = operator.add(&operator.add(&sa, &e), m);
    lwe_out.as_mut()[0] = b;
}

pub fn decrypt_lwe<Ro: Row, Op: ArithmeticOps<Element = Ro::Element>, S>(
    lwe_ct: &Ro,
    s: &[S],
    operator: &Op,
) -> Ro::Element
where
    Ro: TryConvertFrom<[S], Parameters = Ro::Element>,
    Ro::Element: Zero,
{
    let s = Ro::try_convert_from(s, &operator.modulus());

    let mut sa = Ro::Element::zero();
    izip!(lwe_ct.as_ref().iter().skip(1), s.as_ref()).for_each(|(ai, si)| {
        let tmp = operator.mul(ai, si);
        sa = operator.add(&tmp, &sa);
    });

    let b = &lwe_ct.as_ref()[0];
    operator.sub(b, &sa)
}

#[cfg(test)]
mod tests {

    use crate::{
        backend::{ModInit, ModularOpsU64},
        decomposer::{gadget_vector, DefaultDecomposer},
        lwe::lwe_key_switch,
        random::DefaultSecureRng,
        Secret,
    };

    use super::{decrypt_lwe, encrypt_lwe, lwe_ksk_keygen, LweSecret};

    #[test]
    fn encrypt_decrypt_works() {
        let logq = 20;
        let q = 1u64 << logq;
        let lwe_n = 1024;
        let logp = 3;

        let modq_op = ModularOpsU64::new(q);
        let lwe_sk = LweSecret::random(lwe_n >> 1, lwe_n);

        let mut rng = DefaultSecureRng::new();

        // encrypt
        for m in 0..1u64 << logp {
            let encoded_m = m << (logq - logp);
            let mut lwe_ct = vec![0u64; lwe_n + 1];
            encrypt_lwe(
                &mut lwe_ct,
                &encoded_m,
                &lwe_sk.values(),
                &modq_op,
                &mut rng,
            );
            let encoded_m_back = decrypt_lwe(&lwe_ct, &lwe_sk.values(), &modq_op);
            let m_back = ((((encoded_m_back as f64) * ((1 << logp) as f64)) / q as f64).round()
                as u64)
                % (1u64 << logp);
            assert_eq!(m, m_back, "Expected {m} but got {m_back}");
        }
    }

    #[test]
    fn key_switch_works() {
        let logq = 16;
        let logp = 3;
        let q = 1u64 << logq;
        let lwe_in_n = 1024;
        let lwe_out_n = 470;
        let d_ks = 3;
        let logb = 4;

        let lwe_sk_in = LweSecret::random(lwe_in_n >> 1, lwe_in_n);
        let lwe_sk_out = LweSecret::random(lwe_out_n >> 1, lwe_out_n);

        let mut rng = DefaultSecureRng::new();
        let modq_op = ModularOpsU64::new(q);

        // genrate ksk
        for _ in 0..10 {
            let mut ksk = vec![vec![0u64; lwe_out_n + 1]; d_ks * lwe_in_n];
            let gadget = gadget_vector(logq, logb, d_ks);
            lwe_ksk_keygen(
                &lwe_sk_in.values(),
                &lwe_sk_out.values(),
                &mut ksk,
                &gadget,
                &modq_op,
                &mut rng,
            );
            // println!("{:?}", ksk);

            for m in 0..(1 << logp) {
                // encrypt using lwe_sk_in
                let encoded_m = m << (logq - logp);
                let mut lwe_in_ct = vec![0u64; lwe_in_n + 1];
                encrypt_lwe(
                    &mut lwe_in_ct,
                    &encoded_m,
                    lwe_sk_in.values(),
                    &modq_op,
                    &mut rng,
                );

                // key switch from lwe_sk_in to lwe_sk_out
                let decomposer = DefaultDecomposer::new(1u64 << logq, logb, d_ks);
                let mut lwe_out_ct = vec![0u64; lwe_out_n + 1];
                lwe_key_switch(&mut lwe_out_ct, &lwe_in_ct, &ksk, &modq_op, &decomposer);

                // decrypt lwe_out_ct using lwe_sk_out
                let encoded_m_back = decrypt_lwe(&lwe_out_ct, &lwe_sk_out.values(), &modq_op);
                let m_back = ((((encoded_m_back as f64) * ((1 << logp) as f64)) / q as f64).round()
                    as u64)
                    % (1u64 << logp);
                assert_eq!(m, m_back, "Expected {m} but got {m_back}");
                // dbg!(m, m_back);
                // dbg!(encoded_m, encoded_m_back);
            }
        }
    }
}
