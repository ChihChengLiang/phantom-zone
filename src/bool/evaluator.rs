use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    fmt::{Debug, Display},
    marker::PhantomData,
};

use itertools::{izip, partition, Itertools};
use num_traits::{FromPrimitive, Num, One, PrimInt, ToPrimitive, WrappingSub, Zero};

use crate::{
    backend::{ArithmeticOps, ModInit, ModularOpsU64, VectorOps},
    bool::parameters::{MP_BOOL_PARAMS, SP_BOOL_PARAMS},
    decomposer::{Decomposer, DefaultDecomposer, NumInfo, RlweDecomposer},
    lwe::{decrypt_lwe, encrypt_lwe, lwe_key_switch, lwe_ksk_keygen, measure_noise_lwe, LweSecret},
    multi_party::public_key_share,
    ntt::{self, Ntt, NttBackendU64, NttInit},
    random::{DefaultSecureRng, NewWithSeed, RandomGaussianDist, RandomUniformDist},
    rgsw::{
        decrypt_rlwe, galois_auto, galois_key_gen, generate_auto_map, public_key_encrypt_rgsw,
        rgsw_by_rgsw_inplace, rlwe_by_rgsw, secret_key_encrypt_rgsw, IsTrivial, RgswCiphertext,
        RlweCiphertext, RlweSecret,
    },
    utils::{
        fill_random_ternary_secret_with_hamming_weight, generate_prime, mod_exponent,
        TryConvertFrom, WithLocal,
    },
    Matrix, MatrixEntity, MatrixMut, Row, RowEntity, RowMut, Secret,
};

use super::parameters::BoolParameters;

thread_local! {
    static BOOL_EVALUATOR: RefCell<BoolEvaluator<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>> = RefCell::new(BoolEvaluator::new(MP_BOOL_PARAMS));
}

pub fn set_parameter_set(parameter: &BoolParameters<u64>) {
    BoolEvaluator::with_local_mut(|e| *e = BoolEvaluator::new(parameter.clone()))
}

impl WithLocal for BoolEvaluator<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64> {
    fn with_local<F, R>(func: F) -> R
    where
        F: Fn(&Self) -> R,
    {
        BOOL_EVALUATOR.with_borrow(|s| func(s))
    }

    fn with_local_mut<F, R>(func: F) -> R
    where
        F: Fn(&mut Self) -> R,
    {
        BOOL_EVALUATOR.with_borrow_mut(|s| func(s))
    }
}

struct ScratchMemory<M>
where
    M: Matrix,
{
    lwe_vector: M::R,
    decomposition_matrix: M,
}

impl<M: MatrixEntity> ScratchMemory<M>
where
    M::R: RowEntity,
{
    fn new(parameters: &BoolParameters<M::MatElement>) -> Self {
        // Vector to store LWE ciphertext with LWE dimesnion n
        let lwe_vector = M::R::zeros(parameters.lwe_n().0 + 1);

        // Matrix to store decomposed polynomials
        // Max decompistion count + space for temporary RLWE
        let d = std::cmp::max(
            parameters.auto_decomposition_count().0,
            std::cmp::max(
                parameters.rlwe_rgsw_decomposition_count().0 .0,
                parameters.rlwe_rgsw_decomposition_count().1 .0,
            ),
        ) + 2;
        let decomposition_matrix = M::zeros(d, parameters.rlwe_n().0);

        Self {
            lwe_vector,
            decomposition_matrix,
        }
    }
}

// thread_local! {
//     pub(crate) static CLIENT_KEY: RefCell<ClientKey> =
// RefCell::new(ClientKey::random()); }

trait PbsKey {
    type M: Matrix;

    /// RGSW ciphertext of LWE secret elements
    fn rgsw_ct_lwe_si(&self, si: usize) -> &Self::M;
    /// Key for automorphism
    fn galois_key_for_auto(&self, k: isize) -> &Self::M;
    /// LWE ksk to key switch from RLWE secret to LWE secret
    fn lwe_ksk(&self) -> &Self::M;
}

trait PbsInfo {
    type Element;
    type ModOp: VectorOps<Element = Self::Element> + ArithmeticOps<Element = Self::Element>;
    type NttOp: Ntt<Element = Self::Element>;
    type D: Decomposer<Element = Self::Element>;
    fn rlwe_q(&self) -> Self::Element;
    fn lwe_q(&self) -> Self::Element;
    fn br_q(&self) -> usize;
    fn rlwe_n(&self) -> usize;
    fn lwe_n(&self) -> usize;
    /// Embedding fator for ring X^{q}+1 inside
    fn embedding_factor(&self) -> usize;
    /// generator g
    fn g(&self) -> isize;
    /// Decomposers
    fn lwe_decomposer(&self) -> &Self::D;
    fn rlwe_rgsw_decomposer(&self) -> &(Self::D, Self::D);
    fn auto_decomposer(&self) -> &Self::D;

    /// Modulus operators
    fn modop_lweq(&self) -> &Self::ModOp;
    fn modop_rlweq(&self) -> &Self::ModOp;

    /// Ntt operators
    fn nttop_rlweq(&self) -> &Self::NttOp;

    /// Maps a \in Z^*_{q} to discrete log k, with generator g (i.e. g^k =
    /// a). Returned vector is of size q that stores dlog of a at `vec[a]`.
    /// For any a, if k is s.t. a = g^{k}, then k is expressed as k. If k is s.t
    /// a = -g^{k}, then k is expressed as k=k+q/2
    fn g_k_dlog_map(&self) -> &[usize];
    fn rlwe_auto_map(&self, k: isize) -> &(Vec<usize>, Vec<bool>);
}

#[derive(Clone)]
struct ClientKey {
    sk_rlwe: RlweSecret,
    sk_lwe: LweSecret,
}

impl ClientKey {
    fn random() -> Self {
        let sk_rlwe = RlweSecret::random(0, 0);
        let sk_lwe = LweSecret::random(0, 0);
        Self { sk_rlwe, sk_lwe }
    }
}

// impl WithLocal for ClientKey {
//     fn with_local<F, R>(func: F) -> R
//     where
//         F: Fn(&Self) -> R,
//     {
//         CLIENT_KEY.with_borrow(|client_key| func(client_key))
//     }

//     fn with_local_mut<F, R>(func: F) -> R
//     where
//         F: Fn(&mut Self) -> R,
//     {
//         CLIENT_KEY.with_borrow_mut(|client_key| func(client_key))
//     }
// }

// fn set_client_key(key: &ClientKey) {
//     ClientKey::with_local_mut(|k| *k = key.clone())
// }

struct MultiPartyDecryptionShare<E> {
    share: E,
}

struct CommonReferenceSeededCollectivePublicKeyShare<R, S, P> {
    share: R,
    cr_seed: S,
    parameters: P,
}

struct PublicKey<M, R, O> {
    key: M,
    _phantom: PhantomData<(R, O)>,
}

impl<
        M: MatrixMut + MatrixEntity,
        Rng: NewWithSeed + RandomUniformDist<[M::MatElement], Parameters = M::MatElement>,
        ModOp: VectorOps<Element = M::MatElement> + ModInit<Element = M::MatElement>,
    >
    From<
        &[CommonReferenceSeededCollectivePublicKeyShare<
            M::R,
            Rng::Seed,
            BoolParameters<M::MatElement>,
        >],
    > for PublicKey<M, Rng, ModOp>
where
    <M as Matrix>::R: RowMut,
    Rng::Seed: Copy + PartialEq,
    M::MatElement: PartialEq + Copy,
{
    fn from(
        value: &[CommonReferenceSeededCollectivePublicKeyShare<
            M::R,
            Rng::Seed,
            BoolParameters<M::MatElement>,
        >],
    ) -> Self {
        assert!(value.len() > 0);

        let parameters = &value[0].parameters;
        let mut key = M::zeros(2, parameters.rlwe_n().0);

        // sample A
        let seed = value[0].cr_seed;
        let mut main_rng = Rng::new_with_seed(seed);
        RandomUniformDist::random_fill(&mut main_rng, &parameters.rlwe_q().0, key.get_row_mut(0));

        // Sum all Bs
        let rlweq_modop = ModOp::new(parameters.rlwe_q().0);
        value.iter().for_each(|share_i| {
            assert!(share_i.cr_seed == seed);
            assert!(&share_i.parameters == parameters);

            rlweq_modop.elwise_add_mut(key.get_row_mut(1), share_i.share.as_ref());
        });

        PublicKey {
            key,
            _phantom: PhantomData,
        }
    }
}

struct CommonReferenceSeededMultiPartyServerKeyShare<M: Matrix, P, S> {
    rgsw_cts: Vec<M>,
    auto_keys: HashMap<isize, M>,
    lwe_ksk: M::R,
    /// Common reference seed
    cr_seed: S,
    parameters: P,
}
struct SeededMultiPartyServerKey<M: Matrix, S, P> {
    rgsw_cts: Vec<M>,
    auto_keys: HashMap<isize, M>,
    lwe_ksk: M::R,
    cr_seed: S,
    parameters: P,
}

fn aggregate_multi_party_server_key_shares<
    M: MatrixMut + MatrixEntity,
    S: Copy + PartialEq,
    D: RlweDecomposer<Element = M::MatElement>,
    ModOp: VectorOps<Element = M::MatElement> + ModInit<Element = M::MatElement>,
    NttOp: Ntt<Element = M::MatElement> + NttInit<Element = M::MatElement>,
>(
    shares: &[CommonReferenceSeededMultiPartyServerKeyShare<M, BoolParameters<M::MatElement>, S>],
) -> SeededMultiPartyServerKey<M, S, BoolParameters<M::MatElement>>
where
    <M as Matrix>::R: RowMut + RowEntity,
    M::MatElement: Copy + PartialEq + Zero,
    M: Clone,
{
    assert!(shares.len() > 0);
    let parameters = shares[0].parameters.clone();
    let cr_seed = shares[0].cr_seed;

    let rlwe_n = parameters.rlwe_n().0;
    let g = parameters.g() as isize;
    let rlwe_q = parameters.rlwe_q().0;
    let lwe_q = parameters.lwe_q().0;

    // sanity checks
    shares.iter().skip(1).for_each(|s| {
        assert!(s.parameters == parameters);
        assert!(s.cr_seed == cr_seed);
    });

    let rlweq_modop = ModOp::new(rlwe_q);
    let rlweq_nttop = NttOp::new(rlwe_q, rlwe_n);

    // auto keys
    let mut auto_keys = HashMap::new();
    for i in [g, -g] {
        let mut key = M::zeros(parameters.auto_decomposition_count().0, rlwe_n);

        shares.iter().for_each(|s| {
            let auto_key_share_i = s.auto_keys.get(&i).expect("Auto key {i} missing");
            assert!(
                auto_key_share_i.dimension() == (parameters.auto_decomposition_count().0, rlwe_n)
            );
            izip!(key.iter_rows_mut(), auto_key_share_i.iter_rows()).for_each(
                |(partb_out, partb_share)| {
                    rlweq_modop.elwise_add_mut(partb_out.as_mut(), partb_share.as_ref());
                },
            );
        });

        auto_keys.insert(i, key);
    }

    // rgsw ciphertext (most expensive part!)
    let lwe_n = parameters.lwe_n().0;
    let rgsw_by_rgsw_decomposer = parameters.rgsw_rgsw_decomposer::<D::D>();
    let mut scratch_matrix = M::zeros(
        std::cmp::max(
            rgsw_by_rgsw_decomposer.a().decomposition_count(),
            rgsw_by_rgsw_decomposer.b().decomposition_count(),
        ) + (rgsw_by_rgsw_decomposer.a().decomposition_count() * 2
            + rgsw_by_rgsw_decomposer.b().decomposition_count() * 2),
        rlwe_n,
    );

    let mut tmp_rgsw = RgswCiphertext::<M>::empty(rlwe_n, &rgsw_by_rgsw_decomposer, rlwe_q).data;
    let rgsw_cts = (0..lwe_n)
        .into_iter()
        .map(|index| {
            // copy over rgsw ciphertext for index^th secret element from first share and
            // treat it as accumulating rgsw ciphertext
            let mut rgsw_i = shares[0].rgsw_cts[index].clone();

            shares.iter().skip(1).for_each(|si| {
                // copy over si's RGSW[index] ciphertext and send to evaluation domain
                izip!(tmp_rgsw.iter_rows_mut(), si.rgsw_cts[index].iter_rows()).for_each(
                    |(to_ri, from_ri)| {
                        to_ri.as_mut().copy_from_slice(from_ri.as_ref());
                        rlweq_nttop.forward(to_ri.as_mut())
                    },
                );

                rgsw_by_rgsw_inplace(
                    &mut rgsw_i,
                    &tmp_rgsw,
                    &rgsw_by_rgsw_decomposer,
                    &mut scratch_matrix,
                    &rlweq_nttop,
                    &rlweq_modop,
                );
            });

            rgsw_i
        })
        .collect_vec();

    // LWE ksks
    let mut lwe_ksk = M::R::zeros(rlwe_n * parameters.lwe_decomposition_count().0);
    let lweq_modop = ModOp::new(lwe_q);
    shares.iter().for_each(|si| {
        assert!(si.lwe_ksk.as_ref().len() == rlwe_n * parameters.lwe_decomposition_count().0);
        lweq_modop.elwise_add_mut(lwe_ksk.as_mut(), si.lwe_ksk.as_ref())
    });

    SeededMultiPartyServerKey {
        rgsw_cts,
        auto_keys,
        lwe_ksk,
        cr_seed,
        parameters: parameters,
    }
}

/// Seeded single party server key
struct SeededServerKey<M: Matrix, P, S> {
    /// Rgsw cts of LWE secret elements
    pub(crate) rgsw_cts: Vec<M>,
    /// Auto keys
    pub(crate) auto_keys: HashMap<isize, M>,
    /// LWE ksk to key switching LWE ciphertext from RLWE secret to LWE secret
    pub(crate) lwe_ksk: M::R,
    /// Parameters
    pub(crate) parameters: P,
    /// Main seed
    pub(crate) seed: S,
}

impl<M: Matrix, S> SeededServerKey<M, BoolParameters<M::MatElement>, S> {
    pub(crate) fn from_raw(
        auto_keys: HashMap<isize, M>,
        rgsw_cts: Vec<M>,
        lwe_ksk: M::R,
        parameters: BoolParameters<M::MatElement>,
        seed: S,
    ) -> Self {
        // sanity checks
        auto_keys.iter().for_each(|v| {
            assert!(
                v.1.dimension()
                    == (
                        parameters.auto_decomposition_count().0,
                        parameters.rlwe_n().0
                    )
            )
        });

        let (part_a_d, part_b_d) = parameters.rlwe_rgsw_decomposition_count();
        rgsw_cts.iter().for_each(|v| {
            assert!(v.dimension() == (part_a_d.0 * 2 + part_b_d.0, parameters.rlwe_n().0))
        });
        assert!(
            lwe_ksk.as_ref().len()
                == (parameters.lwe_decomposition_count().0 * parameters.rlwe_n().0)
        );

        SeededServerKey {
            rgsw_cts,
            auto_keys,
            lwe_ksk,
            parameters,
            seed,
        }
    }
}

/// Server key in evaluation domain
struct ServerKeyEvaluationDomain<M, R, N> {
    /// Rgsw cts of LWE secret elements
    rgsw_cts: Vec<M>,
    /// Galois keys
    galois_keys: HashMap<isize, M>,
    /// LWE ksk to key switching LWE ciphertext from RLWE secret to LWE secret
    lwe_ksk: M,
    _phanton: PhantomData<(R, N)>,
}

impl<
        M: MatrixMut + MatrixEntity,
        R: RandomUniformDist<[M::MatElement], Parameters = M::MatElement> + NewWithSeed,
        N: NttInit<Element = M::MatElement> + Ntt<Element = M::MatElement>,
    > From<&SeededServerKey<M, BoolParameters<M::MatElement>, R::Seed>>
    for ServerKeyEvaluationDomain<M, R, N>
where
    <M as Matrix>::R: RowMut,
    M::MatElement: Copy,
    R::Seed: Clone,
{
    fn from(value: &SeededServerKey<M, BoolParameters<M::MatElement>, R::Seed>) -> Self {
        let mut main_prng = R::new_with_seed(value.seed.clone());
        let parameters = &value.parameters;
        let g = parameters.g() as isize;
        let ring_size = value.parameters.rlwe_n().0;
        let lwe_n = value.parameters.lwe_n().0;
        let rlwe_q = value.parameters.rlwe_q().0;
        let lwq_q = value.parameters.lwe_q().0;

        let nttop = N::new(rlwe_q, ring_size);

        // galois keys
        let mut auto_keys = HashMap::new();
        let auto_decomp_count = parameters.auto_decomposition_count().0;
        for i in [g, -g] {
            let seeded_auto_key = value.auto_keys.get(&i).unwrap();
            assert!(seeded_auto_key.dimension() == (auto_decomp_count, ring_size));

            let mut data = M::zeros(auto_decomp_count * 2, ring_size);

            // sample RLWE'_A(-s(X^k))
            data.iter_rows_mut().take(auto_decomp_count).for_each(|ri| {
                RandomUniformDist::random_fill(&mut main_prng, &rlwe_q, ri.as_mut())
            });

            // copy over RLWE'B_(-s(X^k))
            izip!(
                data.iter_rows_mut().skip(auto_decomp_count),
                seeded_auto_key.iter_rows()
            )
            .for_each(|(to_ri, from_ri)| to_ri.as_mut().copy_from_slice(from_ri.as_ref()));

            // Send to Evaluation domain
            data.iter_rows_mut()
                .for_each(|ri| nttop.forward(ri.as_mut()));

            auto_keys.insert(i, data);
        }

        // RGSW ciphertexts
        let (rlrg_a_decomp, rlrg_b_decomp) = parameters.rlwe_rgsw_decomposition_count();
        let rgsw_cts = value
            .rgsw_cts
            .iter()
            .map(|seeded_rgsw_si| {
                assert!(
                    seeded_rgsw_si.dimension()
                        == (rlrg_a_decomp.0 * 2 + rlrg_b_decomp.0, ring_size)
                );

                let mut data = M::zeros(rlrg_a_decomp.0 * 2 + rlrg_b_decomp.0 * 2, ring_size);

                // copy over RLWE'(-sm)
                izip!(
                    data.iter_rows_mut().take(rlrg_a_decomp.0 * 2),
                    seeded_rgsw_si.iter_rows().take(rlrg_a_decomp.0 * 2)
                )
                .for_each(|(to_ri, from_ri)| to_ri.as_mut().copy_from_slice(from_ri.as_ref()));

                // sample RLWE'_A(m)
                data.iter_rows_mut()
                    .skip(rlrg_a_decomp.0 * 2)
                    .take(rlrg_b_decomp.0)
                    .for_each(|ri| {
                        RandomUniformDist::random_fill(&mut main_prng, &rlwe_q, ri.as_mut())
                    });

                // copy over RLWE'_B(m)
                izip!(
                    data.iter_rows_mut()
                        .skip(rlrg_a_decomp.0 * 2 + rlrg_b_decomp.0),
                    seeded_rgsw_si.iter_rows().skip(rlrg_a_decomp.0 * 2)
                )
                .for_each(|(to_ri, from_ri)| to_ri.as_mut().copy_from_slice(from_ri.as_ref()));

                // send polynomials to evaluation domain
                data.iter_rows_mut()
                    .for_each(|ri| nttop.forward(ri.as_mut()));

                data
            })
            .collect_vec();

        // LWE ksk
        let lwe_ksk = {
            let d = parameters.lwe_decomposition_count().0;
            assert!(value.lwe_ksk.as_ref().len() == d * ring_size);

            let mut data = M::zeros(d * ring_size, lwe_n + 1);
            izip!(data.iter_rows_mut(), value.lwe_ksk.as_ref().iter()).for_each(|(lwe_i, bi)| {
                RandomUniformDist::random_fill(&mut main_prng, &lwq_q, &mut lwe_i.as_mut()[1..]);
                lwe_i.as_mut()[0] = *bi;
            });

            data
        };

        ServerKeyEvaluationDomain {
            rgsw_cts,
            galois_keys: auto_keys,
            lwe_ksk,
            _phanton: PhantomData,
        }
    }
}

impl<
        M: MatrixMut + MatrixEntity,
        Rng: NewWithSeed,
        N: NttInit<Element = M::MatElement> + Ntt<Element = M::MatElement>,
    > From<&SeededMultiPartyServerKey<M, Rng::Seed, BoolParameters<M::MatElement>>>
    for ServerKeyEvaluationDomain<M, Rng, N>
where
    <M as Matrix>::R: RowMut,
    Rng::Seed: Copy,
    Rng: RandomUniformDist<[M::MatElement], Parameters = M::MatElement>,
    M::MatElement: Copy,
{
    fn from(
        value: &SeededMultiPartyServerKey<M, Rng::Seed, BoolParameters<M::MatElement>>,
    ) -> Self {
        let g = value.parameters.g() as isize;
        let rlwe_n = value.parameters.rlwe_n().0;
        let lwe_n = value.parameters.lwe_n().0;
        let rlwe_q = value.parameters.rlwe_q().0;
        let lwe_q = value.parameters.lwe_q().0;

        let mut main_prng = Rng::new_with_seed(value.cr_seed);

        let rlwe_nttop = N::new(rlwe_q, rlwe_n);

        // auto keys
        let mut auto_keys = HashMap::new();
        let auto_d_count = value.parameters.auto_decomposition_count().0;
        for i in [g, -g] {
            let mut key = M::zeros(auto_d_count * 2, rlwe_n);

            // sample a
            key.iter_rows_mut().take(auto_d_count).for_each(|ri| {
                RandomUniformDist::random_fill(&mut main_prng, &rlwe_q, ri.as_mut())
            });

            let key_part_b = value.auto_keys.get(&i).unwrap();
            assert!(key_part_b.dimension() == (auto_d_count, rlwe_n));
            izip!(
                key.iter_rows_mut().skip(auto_d_count),
                key_part_b.iter_rows()
            )
            .for_each(|(to_ri, from_ri)| {
                to_ri.as_mut().copy_from_slice(from_ri.as_ref());
            });

            // send to evaluation domain
            key.iter_rows_mut()
                .for_each(|ri| rlwe_nttop.forward(ri.as_mut()));

            auto_keys.insert(i, key);
        }

        // rgsw cts
        let (rgswrgsw_d_a, rgswrgsw_d_b) = value.parameters.rgsw_rgsw_decomposition_count();
        let (rlrg_d_a, rlrg_d_b) = value.parameters.rlwe_rgsw_decomposition_count();
        // d_a and d_b may differ for RGSWxRGSW multiplication and RLWExRGSW
        // multiplication. After this point RGSW ciphertexts will only be used for
        // RLWExRGSW multiplication (in blind rotation). Thus we drop any additional
        // RLWE ciphertexts in incoming collective RGSW ciphertexts, which are
        // result of RGSWxRGSW multiplications.
        let rgsw_ct_rows_in = rgswrgsw_d_a.0 * 2 + rgswrgsw_d_b.0 * 2;
        let rgsw_ct_rows_out = rlrg_d_a.0 * 2 + rlrg_d_b.0 * 2;
        let rgsw_cts = value
            .rgsw_cts
            .iter()
            .map(|ct_i_in| {
                assert!(ct_i_in.dimension() == (rgsw_ct_rows_in, rlwe_n));
                let mut eval_ct_i_out = M::zeros(rgsw_ct_rows_out, rlwe_n);

                // RLWE'(-sm) part A
                izip!(
                    eval_ct_i_out.iter_rows_mut().take(rlrg_d_a.0),
                    ct_i_in.iter_rows().take(rlrg_d_a.0)
                )
                .for_each(|(to_ri, from_ri)| {
                    to_ri.as_mut().copy_from_slice(from_ri.as_ref());
                    rlwe_nttop.forward(to_ri.as_mut());
                });

                // RLWE'(-sm) part B
                izip!(
                    eval_ct_i_out
                        .iter_rows_mut()
                        .skip(rlrg_d_a.0)
                        .take(rlrg_d_a.0),
                    ct_i_in.iter_rows().skip(rgswrgsw_d_a.0).take(rlrg_d_a.0)
                )
                .for_each(|(to_ri, from_ri)| {
                    to_ri.as_mut().copy_from_slice(from_ri.as_ref());
                    rlwe_nttop.forward(to_ri.as_mut());
                });

                // RLWE'(m) Part A
                izip!(
                    eval_ct_i_out
                        .iter_rows_mut()
                        .skip(rlrg_d_a.0 * 2)
                        .take(rlrg_d_b.0),
                    ct_i_in
                        .iter_rows()
                        .skip(rgswrgsw_d_a.0 * 2)
                        .take(rlrg_d_b.0)
                )
                .for_each(|(to_ri, from_ri)| {
                    to_ri.as_mut().copy_from_slice(from_ri.as_ref());
                    rlwe_nttop.forward(to_ri.as_mut());
                });

                // RLWE'(m) Part B
                izip!(
                    eval_ct_i_out
                        .iter_rows_mut()
                        .skip(rlrg_d_a.0 * 2 + rlrg_d_b.0)
                        .take(rlrg_d_b.0),
                    ct_i_in
                        .iter_rows()
                        .skip(rgswrgsw_d_a.0 * 2 + rgswrgsw_d_b.0)
                        .take(rlrg_d_b.0)
                )
                .for_each(|(to_ri, from_ri)| {
                    to_ri.as_mut().copy_from_slice(from_ri.as_ref());
                    rlwe_nttop.forward(to_ri.as_mut());
                });

                eval_ct_i_out
            })
            .collect_vec();

        // lwe ksk
        let d_lwe = value.parameters.lwe_decomposition_count().0;
        let mut lwe_ksk = M::zeros(rlwe_n * d_lwe, lwe_n + 1);
        izip!(lwe_ksk.iter_rows_mut(), value.lwe_ksk.as_ref().iter()).for_each(|(lwe_i, bi)| {
            RandomUniformDist::random_fill(&mut main_prng, &lwe_q, &mut lwe_i.as_mut()[1..]);
            lwe_i.as_mut()[0] = *bi;
        });

        ServerKeyEvaluationDomain {
            rgsw_cts,
            galois_keys: auto_keys,
            lwe_ksk,
            _phanton: PhantomData,
        }
    }
}

//FIXME(Jay): Figure out a way for BoolEvaluator to have access to ServerKey
// via a pointer and implement PbsKey for BoolEvaluator instead of ServerKey
// directly
impl<M: Matrix, R, N> PbsKey for ServerKeyEvaluationDomain<M, R, N> {
    type M = M;
    fn galois_key_for_auto(&self, k: isize) -> &Self::M {
        self.galois_keys.get(&k).unwrap()
    }
    fn rgsw_ct_lwe_si(&self, si: usize) -> &Self::M {
        &self.rgsw_cts[si]
    }

    fn lwe_ksk(&self) -> &Self::M {
        &self.lwe_ksk
    }
}

struct BoolPbsInfo<M: Matrix, Ntt, ModOp> {
    auto_decomposer: DefaultDecomposer<M::MatElement>,
    rlwe_rgsw_decomposer: (
        DefaultDecomposer<M::MatElement>,
        DefaultDecomposer<M::MatElement>,
    ),
    lwe_decomposer: DefaultDecomposer<M::MatElement>,
    g_k_dlog_map: Vec<usize>,
    rlwe_nttop: Ntt,
    rlwe_modop: ModOp,
    lwe_modop: ModOp,
    embedding_factor: usize,
    nand_test_vec: M::R,
    rlweq_by8: M::MatElement,
    rlwe_qby4: M::MatElement,
    rlwe_auto_maps: Vec<(Vec<usize>, Vec<bool>)>,
    parameters: BoolParameters<M::MatElement>,
}

impl<M: Matrix, NttOp, ModOp> PbsInfo for BoolPbsInfo<M, NttOp, ModOp>
where
    M::MatElement: PrimInt + WrappingSub + NumInfo + Debug,
    ModOp: ArithmeticOps<Element = M::MatElement> + VectorOps<Element = M::MatElement>,
    NttOp: Ntt<Element = M::MatElement>,
{
    type Element = M::MatElement;
    type D = DefaultDecomposer<M::MatElement>;
    type ModOp = ModOp;
    type NttOp = NttOp;
    fn rlwe_auto_map(&self, k: isize) -> &(Vec<usize>, Vec<bool>) {
        let g = self.parameters.g() as isize;
        if k == g {
            &self.rlwe_auto_maps[0]
        } else if k == -g {
            &self.rlwe_auto_maps[1]
        } else {
            panic!("RLWE auto map only supports k in [-g, g], but got k={k}");
        }
    }
    fn br_q(&self) -> usize {
        self.parameters.br_q().0.to_usize().unwrap()
    }
    fn lwe_decomposer(&self) -> &Self::D {
        &self.lwe_decomposer
    }
    fn rlwe_rgsw_decomposer(&self) -> &(Self::D, Self::D) {
        &self.rlwe_rgsw_decomposer
    }
    fn auto_decomposer(&self) -> &Self::D {
        &self.auto_decomposer
    }
    fn embedding_factor(&self) -> usize {
        self.embedding_factor
    }
    fn g(&self) -> isize {
        self.parameters.g() as isize
    }
    fn g_k_dlog_map(&self) -> &[usize] {
        &self.g_k_dlog_map
    }
    fn lwe_n(&self) -> usize {
        self.parameters.lwe_n().0
    }
    fn lwe_q(&self) -> Self::Element {
        self.parameters.lwe_q().0
    }
    fn rlwe_n(&self) -> usize {
        self.parameters.rlwe_n().0
    }
    fn rlwe_q(&self) -> Self::Element {
        self.parameters.rlwe_q().0
    }
    fn modop_lweq(&self) -> &Self::ModOp {
        &self.lwe_modop
    }
    fn modop_rlweq(&self) -> &Self::ModOp {
        &self.rlwe_modop
    }
    fn nttop_rlweq(&self) -> &Self::NttOp {
        &self.rlwe_nttop
    }
}

struct BoolEvaluator<M, Ntt, ModOp>
where
    M: Matrix,
{
    pbs_info: BoolPbsInfo<M, Ntt, ModOp>,
    scratch_memory: ScratchMemory<M>,
    _phantom: PhantomData<M>,
}

impl<M, NttOp, ModOp> BoolEvaluator<M, NttOp, ModOp>
where
    NttOp: NttInit<Element = M::MatElement> + Ntt<Element = M::MatElement>,
    ModOp: ModInit<Element = M::MatElement>
        + ArithmeticOps<Element = M::MatElement>
        + VectorOps<Element = M::MatElement>,
    M::MatElement: PrimInt + Debug + Display + NumInfo + FromPrimitive + WrappingSub,
    M: MatrixEntity + MatrixMut,
    M::R: TryConvertFrom<[i32], Parameters = M::MatElement> + RowEntity + Debug,
    M: TryConvertFrom<[i32], Parameters = M::MatElement>,
    <M as Matrix>::R: RowMut,
    DefaultSecureRng: RandomGaussianDist<[M::MatElement], Parameters = M::MatElement>
        + RandomGaussianDist<M::MatElement, Parameters = M::MatElement>
        + RandomUniformDist<[M::MatElement], Parameters = M::MatElement>
        + NewWithSeed,
{
    fn new(parameters: BoolParameters<M::MatElement>) -> Self {
        //TODO(Jay): Run sanity checks for modulus values in parameters

        // generatr dlog map s.t. g^{k} % q = a, for all a \in Z*_{q}
        let g = parameters.g();
        let q = parameters.br_q().0.to_usize().unwrap();
        let mut g_k_dlog_map = vec![0usize; q];
        for i in 0..q / 2 {
            let v = mod_exponent(g as u64, i as u64, q as u64) as usize;
            // g^i
            g_k_dlog_map[v] = i;
            // -(g^i)
            g_k_dlog_map[q - v] = i + (q / 2);
        }

        let embedding_factor = (2 * parameters.rlwe_n().0) / q;

        let rlwe_nttop = NttOp::new(parameters.rlwe_q().0, parameters.rlwe_n().0);
        let rlwe_modop = ModInit::new(parameters.rlwe_q().0);
        let lwe_modop = ModInit::new(parameters.lwe_q().0);

        // set test vectors
        let rlwe_q = parameters.rlwe_q().0;
        let q = parameters.br_q().0.to_usize().unwrap();
        let qby2 = q >> 1;
        let qby8 = q >> 3;
        let mut nand_test_vec = M::R::zeros(qby2);
        // Q/8 (Q: rlwe_q)
        let rlwe_qby8 = M::MatElement::from_f64((rlwe_q.to_f64().unwrap() / 8.0).round()).unwrap();
        let true_m_el = rlwe_qby8;
        // -Q/8
        let false_m_el = rlwe_q - rlwe_qby8;
        for i in 0..qby2 {
            if i < (3 * qby8) {
                nand_test_vec.as_mut()[i] = true_m_el;
            } else {
                nand_test_vec.as_mut()[i] = false_m_el;
            }
        }

        // v(X) -> v(X^{-g})
        let (auto_map_index, auto_map_sign) = generate_auto_map(qby2, -(g as isize));
        let mut nand_test_vec_autog = M::R::zeros(qby2);
        izip!(
            nand_test_vec.as_ref().iter(),
            auto_map_index.iter(),
            auto_map_sign.iter()
        )
        .for_each(|(v, to_index, to_sign)| {
            if !to_sign {
                // negate
                nand_test_vec_autog.as_mut()[*to_index] = parameters.rlwe_q().0 - *v;
            } else {
                nand_test_vec_autog.as_mut()[*to_index] = *v;
            }
        });

        // auto map indices and sign
        let mut rlwe_auto_maps = vec![];
        let ring_size = parameters.rlwe_n().0;
        let g = parameters.g() as isize;
        for i in [g, -g] {
            rlwe_auto_maps.push(generate_auto_map(ring_size, i))
        }

        let rlwe_qby4 = M::MatElement::from_f64((rlwe_q.to_f64().unwrap() / 4.0).round()).unwrap();

        let scratch_memory = ScratchMemory::new(&parameters);

        let pbs_info = BoolPbsInfo {
            auto_decomposer: parameters.auto_decomposer(),
            lwe_decomposer: parameters.lwe_decomposer(),
            rlwe_rgsw_decomposer: parameters.rlwe_rgsw_decomposer(),
            g_k_dlog_map,
            embedding_factor,
            lwe_modop,
            rlwe_modop,
            rlwe_nttop,
            nand_test_vec: nand_test_vec_autog,
            rlweq_by8: rlwe_qby8,
            rlwe_qby4: rlwe_qby4,
            rlwe_auto_maps,
            parameters: parameters,
        };

        BoolEvaluator {
            pbs_info,
            scratch_memory,
            _phantom: PhantomData,
        }
    }

    fn client_key(&self) -> ClientKey {
        let sk_lwe = LweSecret::random(
            self.pbs_info.parameters.lwe_n().0 >> 1,
            self.pbs_info.parameters.lwe_n().0,
        );
        let sk_rlwe = RlweSecret::random(
            self.pbs_info.parameters.rlwe_n().0 >> 1,
            self.pbs_info.parameters.rlwe_n().0,
        );
        ClientKey { sk_rlwe, sk_lwe }
    }

    fn server_key(
        &self,
        client_key: &ClientKey,
    ) -> SeededServerKey<M, BoolParameters<M::MatElement>, [u8; 32]> {
        DefaultSecureRng::with_local_mut(|rng| {
            let mut main_seed = [0u8; 32];
            rng.fill_bytes(&mut main_seed);

            let mut main_prng = DefaultSecureRng::new_seeded(main_seed);

            let rlwe_n = self.pbs_info.parameters.rlwe_n().0;
            let sk_rlwe = &client_key.sk_rlwe;
            let sk_lwe = &client_key.sk_lwe;

            // generate auto keys -g, g
            let mut auto_keys = HashMap::new();
            let auto_gadget = self.pbs_info.auto_decomposer.gadget_vector();
            let g = self.pbs_info.parameters.g() as isize;
            for i in [g, -g] {
                let mut gk = M::zeros(self.pbs_info.auto_decomposer.decomposition_count(), rlwe_n);
                galois_key_gen(
                    &mut gk,
                    sk_rlwe.values(),
                    i,
                    &auto_gadget,
                    &self.pbs_info.rlwe_modop,
                    &self.pbs_info.rlwe_nttop,
                    &mut main_prng,
                    rng,
                );
                auto_keys.insert(i, gk);
            }

            // generate rgsw ciphertexts RGSW(si) where si is i^th LWE secret element
            let ring_size = self.pbs_info.parameters.rlwe_n().0;
            let rlwe_q = self.pbs_info.parameters.rlwe_q().0;
            let (rlrg_d_a, rlrg_d_b) = (
                self.pbs_info.rlwe_rgsw_decomposer.0.decomposition_count(),
                self.pbs_info.rlwe_rgsw_decomposer.1.decomposition_count(),
            );
            let rlrg_gadget_a = self.pbs_info.rlwe_rgsw_decomposer.0.gadget_vector();
            let rlrg_gadget_b = self.pbs_info.rlwe_rgsw_decomposer.1.gadget_vector();
            let rgsw_cts = sk_lwe
                .values()
                .iter()
                .map(|si| {
                    // X^{si}; assume |emebedding_factor * si| < N
                    let mut m = M::R::zeros(ring_size);
                    let si = (self.pbs_info.embedding_factor as i32) * si;
                    // dbg!(si);
                    if si < 0 {
                        // X^{-i} = X^{2N - i} = -X^{N-i}
                        m.as_mut()[ring_size - (si.abs() as usize)] = rlwe_q - M::MatElement::one();
                    } else {
                        // X^{i}
                        m.as_mut()[si.abs() as usize] = M::MatElement::one();
                    }

                    let mut rgsw_si = M::zeros(rlrg_d_a * 2 + rlrg_d_b, ring_size);
                    secret_key_encrypt_rgsw(
                        &mut rgsw_si,
                        m.as_ref(),
                        &rlrg_gadget_a,
                        &rlrg_gadget_b,
                        sk_rlwe.values(),
                        &self.pbs_info.rlwe_modop,
                        &self.pbs_info.rlwe_nttop,
                        &mut main_prng,
                        rng,
                    );

                    rgsw_si
                })
                .collect_vec();

            // LWE KSK from RLWE secret s -> LWE secret z
            let d_lwe_gadget = self.pbs_info.lwe_decomposer.gadget_vector();
            let mut lwe_ksk =
                M::R::zeros(self.pbs_info.lwe_decomposer.decomposition_count() * ring_size);
            lwe_ksk_keygen(
                &sk_rlwe.values(),
                &sk_lwe.values(),
                &mut lwe_ksk,
                &d_lwe_gadget,
                &self.pbs_info.lwe_modop,
                &mut main_prng,
                rng,
            );

            SeededServerKey::from_raw(
                auto_keys,
                rgsw_cts,
                lwe_ksk,
                self.pbs_info.parameters.clone(),
                main_seed,
            )
        })
    }

    fn multi_party_server_key_share(
        &self,
        cr_seed: [u8; 32],
        collective_pk: &M,
        client_key: &ClientKey,
    ) -> CommonReferenceSeededMultiPartyServerKeyShare<M, BoolParameters<M::MatElement>, [u8; 32]>
    {
        DefaultSecureRng::with_local_mut(|rng| {
            let mut main_prng = DefaultSecureRng::new_seeded(cr_seed);

            let sk_rlwe = &client_key.sk_rlwe;
            let sk_lwe = &client_key.sk_lwe;

            let g = self.pbs_info.parameters.g() as isize;
            let ring_size = self.pbs_info.parameters.rlwe_n().0;
            let rlwe_q = self.pbs_info.parameters.rlwe_q().0;
            let lwe_q = self.pbs_info.parameters.lwe_q().0;

            let rlweq_modop = ModOp::new(rlwe_q);
            let rlweq_nttop = NttOp::new(rlwe_q, ring_size);

            // sanity check
            assert!(sk_rlwe.values().len() == ring_size);
            assert!(sk_lwe.values().len() == self.pbs_info.parameters.lwe_n().0);

            // auto keys
            let mut auto_keys = HashMap::new();
            let auto_gadget = self.pbs_info.auto_decomposer.gadget_vector();
            for i in [g, -g] {
                let mut ksk_out = M::zeros(
                    self.pbs_info.auto_decomposer.decomposition_count(),
                    ring_size,
                );
                galois_key_gen(
                    &mut ksk_out,
                    sk_rlwe.values(),
                    i,
                    &auto_gadget,
                    &rlweq_modop,
                    &rlweq_nttop,
                    &mut main_prng,
                    rng,
                );
                auto_keys.insert(i, ksk_out);
            }

            // rgsw ciphertexts of lwe secret elements
            let rgsw_rgsw_decomposer = self
                .pbs_info
                .parameters
                .rgsw_rgsw_decomposer::<DefaultDecomposer<M::MatElement>>();
            let (rgrg_d_a, rgrg_d_b) = (
                rgsw_rgsw_decomposer.0.decomposition_count(),
                rgsw_rgsw_decomposer.1.decomposition_count(),
            );
            let (rgrg_gadget_a, rgrg_gadget_b) = (
                rgsw_rgsw_decomposer.0.gadget_vector(),
                rgsw_rgsw_decomposer.1.gadget_vector(),
            );
            let rgsw_cts = sk_lwe
                .values()
                .iter()
                .map(|si| {
                    let mut m = M::R::zeros(ring_size);
                    //TODO(Jay): It will be nice to have a function that returns polynomial
                    // (monomial infact!) corresponding to secret element embedded in ring X^{2N+1}.
                    // Save lots of mistakes where one forgest to emebed si in bigger ring.
                    let si = *si * (self.pbs_info.embedding_factor as i32);
                    if si < 0 {
                        // X^{-si} = X^{2N-si} = -X^{N-si}, assuming abs(si) < N
                        // (which it is given si is secret element)
                        m.as_mut()[ring_size - (si.abs() as usize)] = rlwe_q - M::MatElement::one();
                    } else {
                        m.as_mut()[si as usize] = M::MatElement::one();
                    }

                    // public key RGSW encryption has no part that can be seeded, unlike secret key
                    // RGSW encryption where RLWE'_A(m) is seeded
                    let mut out_rgsw = M::zeros(rgrg_d_a * 2 + rgrg_d_b * 2, ring_size);
                    public_key_encrypt_rgsw(
                        &mut out_rgsw,
                        &m.as_ref(),
                        collective_pk,
                        &rgrg_gadget_a,
                        &rgrg_gadget_b,
                        &rlweq_modop,
                        &rlweq_nttop,
                        rng,
                    );

                    out_rgsw
                })
                .collect_vec();

            // LWE ksk
            let mut lwe_ksk =
                M::R::zeros(self.pbs_info.lwe_decomposer.decomposition_count() * ring_size);
            let lwe_modop = ModOp::new(lwe_q);
            let d_lwe_gadget_vec = self.pbs_info.lwe_decomposer.gadget_vector();
            lwe_ksk_keygen(
                sk_rlwe.values(),
                sk_lwe.values(),
                &mut lwe_ksk,
                &d_lwe_gadget_vec,
                &lwe_modop,
                &mut main_prng,
                rng,
            );

            CommonReferenceSeededMultiPartyServerKeyShare {
                auto_keys,
                rgsw_cts,
                lwe_ksk,
                cr_seed,
                parameters: self.pbs_info.parameters.clone(),
            }
        })
    }

    fn multi_party_public_key_share(
        &self,
        cr_seed: [u8; 32],
        client_key: &ClientKey,
    ) -> CommonReferenceSeededCollectivePublicKeyShare<
        <M as Matrix>::R,
        [u8; 32],
        BoolParameters<<M as Matrix>::MatElement>,
    > {
        DefaultSecureRng::with_local_mut(|rng| {
            let mut share_out = M::R::zeros(self.pbs_info.parameters.rlwe_n().0);
            let modop = ModOp::new(self.pbs_info.parameters.rlwe_q().0);
            let nttop = NttOp::new(
                self.pbs_info.parameters.rlwe_q().0,
                self.pbs_info.parameters.rlwe_n().0,
            );
            let mut main_prng = DefaultSecureRng::new_seeded(cr_seed);
            public_key_share(
                &mut share_out,
                client_key.sk_rlwe.values(),
                &modop,
                &nttop,
                &mut main_prng,
                rng,
            );

            CommonReferenceSeededCollectivePublicKeyShare {
                share: share_out,
                cr_seed: cr_seed,
                parameters: self.pbs_info.parameters.clone(),
            }
        })
    }

    fn multi_party_decryption_share(
        &self,
        lwe_ct: &M::R,
        client_key: &ClientKey,
    ) -> MultiPartyDecryptionShare<<M as Matrix>::MatElement> {
        assert!(lwe_ct.as_ref().len() == self.pbs_info.parameters.rlwe_n().0 + 1);
        let modop = &self.pbs_info.rlwe_modop;
        let mut neg_s = M::R::try_convert_from(
            client_key.sk_rlwe.values(),
            &self.pbs_info.parameters.rlwe_q().0,
        );
        modop.elwise_neg_mut(neg_s.as_mut());

        let mut neg_sa = M::MatElement::zero();
        izip!(lwe_ct.as_ref().iter().skip(1), neg_s.as_ref().iter()).for_each(|(ai, nsi)| {
            neg_sa = modop.add(&neg_sa, &modop.mul(ai, nsi));
        });

        let e = DefaultSecureRng::with_local_mut(|rng| {
            let mut e = M::MatElement::zero();
            RandomGaussianDist::random_fill(rng, &self.pbs_info.parameters.rlwe_q().0, &mut e);
            e
        });
        let share = modop.add(&neg_sa, &e);

        MultiPartyDecryptionShare { share }
    }

    pub(crate) fn multi_party_decrypt(
        &self,
        shares: &[MultiPartyDecryptionShare<M::MatElement>],
        lwe_ct: &M::R,
    ) -> bool {
        let modop = &self.pbs_info.rlwe_modop;
        let mut sum_a = M::MatElement::zero();
        shares
            .iter()
            .for_each(|share_i| sum_a = modop.add(&sum_a, &share_i.share));

        let encoded_m = modop.add(&lwe_ct.as_ref()[0], &sum_a);

        let m = (((encoded_m + self.pbs_info.rlweq_by8).to_f64().unwrap() * 4f64)
            / self.pbs_info.parameters.rlwe_q().0.to_f64().unwrap())
        .round() as usize
            % 4usize;

        if m == 0 {
            return false;
        } else if m == 1 {
            return true;
        } else {
            panic!("Bool decryption failure. Expected m to be either 1 or 0, but m={m} ");
        }
    }

    /// First encrypt as RLWE(m) with m as constant polynomial and extract it as
    /// LWE ciphertext
    pub(crate) fn pk_encrypt(&self, pk: &M, m: bool) -> M::R {
        DefaultSecureRng::with_local_mut(|rng| {
            let modop = &self.pbs_info.rlwe_modop;
            let nttop = &self.pbs_info.rlwe_nttop;

            // RLWE(0)
            // sample ephemeral key u
            let ring_size = self.pbs_info.parameters.rlwe_n().0;
            let mut u = vec![0i32; ring_size];
            fill_random_ternary_secret_with_hamming_weight(u.as_mut(), ring_size >> 1, rng);
            let mut u = M::R::try_convert_from(&u, &self.pbs_info.parameters.rlwe_q().0);
            nttop.forward(u.as_mut());

            let mut ua = M::R::zeros(ring_size);
            ua.as_mut().copy_from_slice(pk.get_row_slice(0));
            let mut ub = M::R::zeros(ring_size);
            ub.as_mut().copy_from_slice(pk.get_row_slice(1));

            // a*u
            nttop.forward(ua.as_mut());
            modop.elwise_mul_mut(ua.as_mut(), u.as_ref());
            nttop.backward(ua.as_mut());

            // b*u
            nttop.forward(ub.as_mut());
            modop.elwise_mul_mut(ub.as_mut(), u.as_ref());
            nttop.backward(ub.as_mut());

            let mut rlwe = M::zeros(2, ring_size);
            // sample error
            rlwe.iter_rows_mut().for_each(|ri| {
                RandomGaussianDist::random_fill(
                    rng,
                    &self.pbs_info.parameters.rlwe_q().0,
                    ri.as_mut(),
                );
            });

            // a*u + e0
            modop.elwise_add_mut(rlwe.get_row_mut(0), ua.as_ref());
            // b*u + e1
            modop.elwise_add_mut(rlwe.get_row_mut(1), ub.as_ref());

            let m = if m {
                // Q/8
                self.pbs_info.rlweq_by8
            } else {
                // -Q/8
                self.pbs_info.parameters.rlwe_q().0 - self.pbs_info.rlweq_by8
            };

            // b*u + e1 + m, where m is constant polynomial
            rlwe.set(1, 0, modop.add(rlwe.get(1, 0), &m));

            // sample extract index 0
            let mut lwe_out = M::R::zeros(ring_size + 1);
            sample_extract(&mut lwe_out, &rlwe, modop, 0);

            lwe_out
        })
    }

    /// TODO(Jay): Fetch client key from thread local
    pub fn sk_encrypt(&self, m: bool, client_key: &ClientKey) -> M::R {
        let m = if m {
            // Q/8
            self.pbs_info.rlweq_by8
        } else {
            // -Q/8
            self.pbs_info.parameters.rlwe_q().0 - self.pbs_info.rlweq_by8
        };

        DefaultSecureRng::with_local_mut(|rng| {
            let mut lwe_out = M::R::zeros(self.pbs_info.parameters.rlwe_n().0 + 1);
            encrypt_lwe(
                &mut lwe_out,
                &m,
                client_key.sk_rlwe.values(),
                &self.pbs_info.rlwe_modop,
                rng,
            );
            lwe_out
        })
    }

    pub fn sk_decrypt(&self, lwe_ct: &M::R, client_key: &ClientKey) -> bool {
        let m = decrypt_lwe(
            lwe_ct,
            client_key.sk_rlwe.values(),
            &self.pbs_info.rlwe_modop,
        );
        let m = {
            // m + q/8 => {0,q/4 1}
            (((m + self.pbs_info.rlweq_by8).to_f64().unwrap() * 4.0)
                / self.pbs_info.parameters.rlwe_q().0.to_f64().unwrap())
            .round()
            .to_usize()
            .unwrap()
                % 4
        };

        if m == 0 {
            false
        } else if m == 1 {
            true
        } else {
            panic!("Incorrect bool decryption. Got m={m} but expected m to be 0 or 1")
        }
    }

    // TODO(Jay): scratch spaces must be thread local. Don't pass them as arguments
    pub fn nand(
        &mut self,
        c0: &M::R,
        c1: &M::R,
        server_key: &ServerKeyEvaluationDomain<M, DefaultSecureRng, NttOp>,
    ) -> M::R {
        let mut c_out = M::R::zeros(c0.as_ref().len());
        let modop = &self.pbs_info.rlwe_modop;
        izip!(
            c_out.as_mut().iter_mut(),
            c0.as_ref().iter(),
            c1.as_ref().iter()
        )
        .for_each(|(o, i0, i1)| {
            *o = modop.add(i0, i1);
        });
        // +Q/4
        c_out.as_mut()[0] = modop.add(&c_out.as_ref()[0], &self.pbs_info.rlwe_qby4);

        // PBS
        pbs(
            &self.pbs_info,
            &self.pbs_info.nand_test_vec,
            &mut c_out,
            server_key,
            &mut self.scratch_memory.lwe_vector,
            &mut self.scratch_memory.decomposition_matrix,
        );

        c_out
    }
}

/// LMKCY+ Blind rotation
///
/// gk_to_si: [g^0, ..., g^{q/2-1}, -g^0, -g^1, .., -g^{q/2-1}]
fn blind_rotation<
    MT: IsTrivial + MatrixMut,
    Mmut: MatrixMut<MatElement = MT::MatElement> + Matrix,
    D: Decomposer<Element = MT::MatElement>,
    NttOp: Ntt<Element = MT::MatElement>,
    ModOp: ArithmeticOps<Element = MT::MatElement> + VectorOps<Element = MT::MatElement>,
    K: PbsKey<M = Mmut>,
    P: PbsInfo<Element = MT::MatElement>,
>(
    trivial_rlwe_test_poly: &mut MT,
    scratch_matrix: &mut Mmut,
    g: isize,
    w: usize,
    q: usize,
    gk_to_si: &[Vec<usize>],
    rlwe_rgsw_decomposer: &(D, D),
    auto_decomposer: &D,
    ntt_op: &NttOp,
    mod_op: &ModOp,
    parameters: &P,
    pbs_key: &K,
) where
    <Mmut as Matrix>::R: RowMut,
    Mmut::MatElement: Copy + Zero,
    <MT as Matrix>::R: RowMut,
{
    let q_by_2 = q / 2;

    // -(g^k)
    for i in (1..q_by_2).rev() {
        gk_to_si[q_by_2 + i].iter().for_each(|s_index| {
            rlwe_by_rgsw(
                trivial_rlwe_test_poly,
                pbs_key.rgsw_ct_lwe_si(*s_index),
                scratch_matrix,
                rlwe_rgsw_decomposer,
                ntt_op,
                mod_op,
            );
        });

        let (auto_map_index, auto_map_sign) = parameters.rlwe_auto_map(g);
        galois_auto(
            trivial_rlwe_test_poly,
            pbs_key.galois_key_for_auto(g),
            scratch_matrix,
            &auto_map_index,
            &auto_map_sign,
            mod_op,
            ntt_op,
            auto_decomposer,
        );
    }

    // -(g^0)
    gk_to_si[q_by_2].iter().for_each(|s_index| {
        rlwe_by_rgsw(
            trivial_rlwe_test_poly,
            pbs_key.rgsw_ct_lwe_si(*s_index),
            scratch_matrix,
            rlwe_rgsw_decomposer,
            ntt_op,
            mod_op,
        );
    });
    let (auto_map_index, auto_map_sign) = parameters.rlwe_auto_map(-g);
    galois_auto(
        trivial_rlwe_test_poly,
        pbs_key.galois_key_for_auto(-g),
        scratch_matrix,
        &auto_map_index,
        &auto_map_sign,
        mod_op,
        ntt_op,
        auto_decomposer,
    );

    // +(g^k)
    for i in (1..q_by_2).rev() {
        gk_to_si[i].iter().for_each(|s_index| {
            rlwe_by_rgsw(
                trivial_rlwe_test_poly,
                pbs_key.rgsw_ct_lwe_si(*s_index),
                scratch_matrix,
                rlwe_rgsw_decomposer,
                ntt_op,
                mod_op,
            );
        });

        let (auto_map_index, auto_map_sign) = parameters.rlwe_auto_map(g);
        galois_auto(
            trivial_rlwe_test_poly,
            pbs_key.galois_key_for_auto(g),
            scratch_matrix,
            &auto_map_index,
            &auto_map_sign,
            mod_op,
            ntt_op,
            auto_decomposer,
        );
    }

    // +(g^0)
    gk_to_si[0].iter().for_each(|s_index| {
        rlwe_by_rgsw(
            trivial_rlwe_test_poly,
            pbs_key.rgsw_ct_lwe_si(gk_to_si[q_by_2][*s_index]),
            scratch_matrix,
            rlwe_rgsw_decomposer,
            ntt_op,
            mod_op,
        );
    });
}

/// - Mod down
/// - key switching
/// - mod down
/// - blind rotate
fn pbs<
    M: Matrix + MatrixMut + MatrixEntity,
    P: PbsInfo<Element = M::MatElement>,
    // NttOp: Ntt<Element = M::MatElement>,
    // ModOp: ArithmeticOps<Element = M::MatElement> + VectorOps<Element = M::MatElement>,
    K: PbsKey<M = M>,
>(
    pbs_info: &P,
    test_vec: &M::R,
    lwe_in: &mut M::R,
    pbs_key: &K,
    scratch_lwe_vec: &mut M::R,
    scratch_blind_rotate_matrix: &mut M,
) where
    <M as Matrix>::R: RowMut,
    M::MatElement: PrimInt + ToPrimitive + FromPrimitive + One + Copy + Zero + Display,
{
    let rlwe_q = pbs_info.rlwe_q();
    let lwe_q = pbs_info.lwe_q();
    let br_q = pbs_info.br_q();
    let rlwe_qf64 = rlwe_q.to_f64().unwrap();
    let lwe_qf64 = lwe_q.to_f64().unwrap();
    let br_qf64 = br_q.to_f64().unwrap();
    let rlwe_n = pbs_info.rlwe_n();

    PBSTracer::with_local_mut(|t| {
        let out = lwe_in
            .as_ref()
            .iter()
            .map(|v| v.to_u64().unwrap())
            .collect_vec();
        t.ct_rlwe_q_mod = out;
    });

    // moddown Q -> Q_ks
    lwe_in.as_mut().iter_mut().for_each(|v| {
        *v =
            M::MatElement::from_f64(((v.to_f64().unwrap() * lwe_qf64) / rlwe_qf64).round()).unwrap()
    });

    PBSTracer::with_local_mut(|t| {
        let out = lwe_in
            .as_ref()
            .iter()
            .map(|v| v.to_u64().unwrap())
            .collect_vec();
        t.ct_lwe_q_mod = out;
    });

    // key switch RLWE secret to LWE secret
    scratch_lwe_vec.as_mut().fill(M::MatElement::zero());
    lwe_key_switch(
        scratch_lwe_vec,
        lwe_in,
        pbs_key.lwe_ksk(),
        pbs_info.modop_lweq(),
        pbs_info.lwe_decomposer(),
    );

    PBSTracer::with_local_mut(|t| {
        let out = scratch_lwe_vec
            .as_ref()
            .iter()
            .map(|v| v.to_u64().unwrap())
            .collect_vec();
        t.ct_lwe_q_mod_after_ksk = out;
    });

    // odd mowdown Q_ks -> q
    let g_k_dlog_map = pbs_info.g_k_dlog_map();
    let mut g_k_si = vec![vec![]; br_q];
    scratch_lwe_vec
        .as_ref()
        .iter()
        .skip(1)
        .enumerate()
        .for_each(|(index, v)| {
            let odd_v = mod_switch_odd(v.to_f64().unwrap(), lwe_qf64, br_qf64);
            let k = g_k_dlog_map[odd_v];
            g_k_si[k].push(index);
        });

    PBSTracer::with_local_mut(|t| {
        let out = scratch_lwe_vec
            .as_ref()
            .iter()
            .map(|v| mod_switch_odd(v.to_f64().unwrap(), lwe_qf64, br_qf64) as u64)
            .collect_vec();
        t.ct_br_q_mod = out;
    });

    // handle b and set trivial test RLWE
    let g = pbs_info.g() as usize;
    let g_times_b = (g * mod_switch_odd(
        scratch_lwe_vec.as_ref()[0].to_f64().unwrap(),
        lwe_qf64,
        br_qf64,
    )) % (br_q);
    // v = (v(X) * X^{g*b}) mod X^{q/2}+1
    let br_qby2 = br_q / 2;
    let mut gb_monomial_sign = true;
    let mut gb_monomial_exp = g_times_b;
    // X^{g*b} mod X^{q/2}+1
    if gb_monomial_exp > br_qby2 {
        gb_monomial_exp -= br_qby2;
        gb_monomial_sign = false
    }
    // monomial mul
    let mut trivial_rlwe_test_poly = RlweCiphertext::<_, DefaultSecureRng> {
        data: M::zeros(2, rlwe_n),
        is_trivial: true,
        _phatom: PhantomData,
    };
    if pbs_info.embedding_factor() == 1 {
        monomial_mul(
            test_vec.as_ref(),
            trivial_rlwe_test_poly.get_row_mut(1).as_mut(),
            gb_monomial_exp,
            gb_monomial_sign,
            br_qby2,
            pbs_info.modop_rlweq(),
        );
    } else {
        // use lwe_in to store the `t = v(X) * X^{g*2} mod X^{q/2}+1` temporarily. This
        // works because q/2 <= N (where N is lwe_in LWE dimension) always.
        monomial_mul(
            test_vec.as_ref(),
            &mut lwe_in.as_mut()[..br_qby2],
            gb_monomial_exp,
            gb_monomial_sign,
            br_qby2,
            pbs_info.modop_rlweq(),
        );

        // emebed poly `t` in ring X^{q/2}+1 inside the bigger ring X^{N}+1
        let embed_factor = pbs_info.embedding_factor();
        let partb_trivial_rlwe = trivial_rlwe_test_poly.get_row_mut(1);
        lwe_in.as_ref()[..br_qby2]
            .iter()
            .enumerate()
            .for_each(|(index, v)| {
                partb_trivial_rlwe[embed_factor * index] = *v;
            });
    }

    // blind rotate
    blind_rotation(
        &mut trivial_rlwe_test_poly,
        scratch_blind_rotate_matrix,
        pbs_info.g(),
        1,
        br_q,
        &g_k_si,
        pbs_info.rlwe_rgsw_decomposer(),
        pbs_info.auto_decomposer(),
        pbs_info.nttop_rlweq(),
        pbs_info.modop_rlweq(),
        pbs_info,
        pbs_key,
    );

    // ClientKey::with_local(|ck| {
    //     let ring_size = parameters.rlwe_n();
    //     let mut rlwe_ct = vec![vec![0u64; ring_size]; 2];
    //     izip!(
    //         rlwe_ct[0].iter_mut(),
    //         trivial_rlwe_test_poly.0.get_row_slice(0)
    //     )
    //     .for_each(|(t, f)| {
    //         *t = f.to_u64().unwrap();
    //     });
    //     izip!(
    //         rlwe_ct[1].iter_mut(),
    //         trivial_rlwe_test_poly.0.get_row_slice(1)
    //     )
    //     .for_each(|(t, f)| {
    //         *t = f.to_u64().unwrap();
    //     });
    //     let mut m_out = vec![vec![0u64; ring_size]];
    //     let modop = ModularOpsU64::new(rlwe_q.to_u64().unwrap());
    //     let nttop = NttBackendU64::new(rlwe_q.to_u64().unwrap(), ring_size);
    //     decrypt_rlwe(&rlwe_ct, ck.sk_rlwe.values(), &mut m_out, &nttop, &modop);

    //     println!("RLWE post PBS message: {:?}", m_out[0]);
    // });

    // sample extract
    sample_extract(lwe_in, &trivial_rlwe_test_poly, pbs_info.modop_rlweq(), 0);
}

fn mod_switch_odd(v: f64, from_q: f64, to_q: f64) -> usize {
    let odd_v = (((v * to_q) / (from_q)).floor()).to_usize().unwrap();
    //TODO(Jay): check correctness of this
    odd_v + ((odd_v & 1) ^ 1)
}

// TODO(Jay): Add tests for sample extract
fn sample_extract<M: Matrix + MatrixMut, ModOp: ArithmeticOps<Element = M::MatElement>>(
    lwe_out: &mut M::R,
    rlwe_in: &M,
    mod_op: &ModOp,
    index: usize,
) where
    <M as Matrix>::R: RowMut,
    M::MatElement: Copy,
{
    let ring_size = rlwe_in.dimension().1;

    // index..=0
    let to = &mut lwe_out.as_mut()[1..];
    let from = rlwe_in.get_row_slice(0);
    for i in 0..index + 1 {
        to[i] = from[index - i];
    }

    // -(N..index)
    for i in index + 1..ring_size {
        to[i] = mod_op.neg(&from[ring_size + index - i]);
    }

    // set b
    lwe_out.as_mut()[0] = *rlwe_in.get(1, index);
}

/// TODO(Jay): Write tests for monomial mul
fn monomial_mul<El, ModOp: ArithmeticOps<Element = El>>(
    p_in: &[El],
    p_out: &mut [El],
    mon_exp: usize,
    mon_sign: bool,
    ring_size: usize,
    mod_op: &ModOp,
) where
    El: Copy,
{
    debug_assert!(p_in.as_ref().len() == ring_size);
    debug_assert!(p_in.as_ref().len() == p_out.as_ref().len());
    debug_assert!(mon_exp < ring_size);

    p_in.as_ref().iter().enumerate().for_each(|(index, v)| {
        let mut to_index = index + mon_exp;
        let mut to_sign = mon_sign;
        if to_index >= ring_size {
            to_index = to_index - ring_size;
            to_sign = !to_sign;
        }

        if !to_sign {
            p_out.as_mut()[to_index] = mod_op.neg(v);
        } else {
            p_out.as_mut()[to_index] = *v;
        }
    });
}

thread_local! {
    static PBS_TRACER: RefCell<PBSTracer<Vec<Vec<u64>>>> = RefCell::new(PBSTracer::default());
}

#[derive(Default)]
struct PBSTracer<M>
where
    M: Matrix + Default,
{
    pub(crate) ct_rlwe_q_mod: M::R,
    pub(crate) ct_lwe_q_mod: M::R,
    pub(crate) ct_lwe_q_mod_after_ksk: M::R,
    pub(crate) ct_br_q_mod: Vec<u64>,
}

impl PBSTracer<Vec<Vec<u64>>> {
    fn trace(&self, parameters: &BoolParameters<u64>, sk_lwe: &[i32], sk_rlwe: &[i32]) {
        assert!(parameters.rlwe_n().0 == sk_rlwe.len());
        assert!(parameters.lwe_n().0 == sk_lwe.len());

        let modop_rlweq = ModularOpsU64::new(parameters.rlwe_q().0);
        // noise after mod down Q -> Q_ks
        let m_back0 = decrypt_lwe(&self.ct_rlwe_q_mod, sk_rlwe, &modop_rlweq);

        let modop_lweq = ModularOpsU64::new(parameters.lwe_q().0);
        // noise after mod down Q -> Q_ks
        let m_back1 = decrypt_lwe(&self.ct_lwe_q_mod, sk_rlwe, &modop_lweq);
        // noise after key switch from RLWE -> LWE
        let m_back2 = decrypt_lwe(&self.ct_lwe_q_mod_after_ksk, sk_lwe, &modop_lweq);

        // noise after mod down odd from Q_ks -> q
        let modop_br_q = ModularOpsU64::new(parameters.br_q().0);
        let m_back3 = decrypt_lwe(&self.ct_br_q_mod, sk_lwe, &modop_br_q);

        println!(
            "
            M initial mod Q: {m_back0},
            M after mod down Q -> Q_ks: {m_back1},
            M after key switch from RLWE -> LWE: {m_back2},
            M after mod dwon Q_ks -> q: {m_back3}
        "
        );
    }
}

impl WithLocal for PBSTracer<Vec<Vec<u64>>> {
    fn with_local<F, R>(func: F) -> R
    where
        F: Fn(&Self) -> R,
    {
        PBS_TRACER.with_borrow(|t| func(t))
    }

    fn with_local_mut<F, R>(func: F) -> R
    where
        F: Fn(&mut Self) -> R,
    {
        PBS_TRACER.with_borrow_mut(|t| func(t))
    }
}

#[cfg(test)]
mod tests {
    use std::iter::Sum;

    use rand::{thread_rng, Rng};
    use rand_distr::Uniform;

    use crate::{
        backend::ModularOpsU64,
        bool,
        ntt::NttBackendU64,
        random::DEFAULT_RNG,
        rgsw::{
            self, measure_noise, public_key_encrypt_rlwe, secret_key_encrypt_rlwe,
            tests::{_measure_noise_rgsw, _sk_encrypt_rlwe},
            RgswCiphertext, RgswCiphertextEvaluationDomain, SeededRgswCiphertext,
            SeededRlweCiphertext,
        },
        utils::{negacyclic_mul, Stats},
    };

    use super::*;

    #[test]
    fn bool_encrypt_decrypt_works() {
        let bool_evaluator =
            BoolEvaluator::<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>::new(SP_BOOL_PARAMS);
        let client_key = bool_evaluator.client_key();

        let mut m = true;
        for _ in 0..1000 {
            let lwe_ct = bool_evaluator.sk_encrypt(m, &client_key);
            let m_back = bool_evaluator.sk_decrypt(&lwe_ct, &client_key);
            assert_eq!(m, m_back);
            m = !m;
        }
    }

    #[test]
    fn bool_nand() {
        DefaultSecureRng::with_local_mut(|r| {
            let rng = DefaultSecureRng::new_seeded([19u8; 32]);
            *r = rng;
        });

        let mut bool_evaluator =
            BoolEvaluator::<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>::new(SP_BOOL_PARAMS);

        // println!("{:?}", bool_evaluator.nand_test_vec);
        let client_key = bool_evaluator.client_key();
        let seeded_server_key = bool_evaluator.server_key(&client_key);
        let server_key_eval_domain =
            ServerKeyEvaluationDomain::<_, DefaultSecureRng, NttBackendU64>::from(
                &seeded_server_key,
            );

        let mut m0 = false;
        let mut m1 = true;
        let mut ct0 = bool_evaluator.sk_encrypt(m0, &client_key);
        let mut ct1 = bool_evaluator.sk_encrypt(m1, &client_key);

        for _ in 0..1000 {
            let ct_back = bool_evaluator.nand(&ct0, &ct1, &server_key_eval_domain);

            let m_out = !(m0 && m1);

            // Trace and measure PBS noise
            {
                let noise0 = {
                    let ideal = if m0 {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &ct0,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal,
                    );
                    let v = decrypt_lwe(
                        &ct0,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };
                let noise1 = {
                    let ideal = if m1 {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &ct1,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal,
                    );
                    let v = decrypt_lwe(
                        &ct1,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };

                // Trace PBS
                PBSTracer::with_local(|t| {
                    t.trace(
                        &SP_BOOL_PARAMS,
                        &client_key.sk_lwe.values(),
                        client_key.sk_rlwe.values(),
                    )
                });

                // Calculate noise in ciphertext post PBS
                let noise_out = {
                    let ideal = if m_out {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &ct_back,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal,
                    );
                    let v = decrypt_lwe(
                        &ct_back,
                        client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };
                dbg!(m0, m1, m_out);
                println!(
                    "ct0 (noise, message): {:?}  \n ct1 (noise, message): {:?} \n PBS (noise, message): {:?}", noise0, noise1, noise_out
                );
            }
            let m_back = bool_evaluator.sk_decrypt(&ct_back, &client_key);
            assert!(m_out == m_back, "Expected {m_out}, got {m_back}");
            println!("----------");

            m1 = m0;
            m0 = m_out;

            ct1 = ct0;
            ct0 = ct_back;
        }
    }

    #[test]
    fn multi_party_encryption_decryption() {
        let bool_evaluator =
            BoolEvaluator::<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>::new(MP_BOOL_PARAMS);

        let no_of_parties = 500;
        let parties = (0..no_of_parties)
            .map(|_| bool_evaluator.client_key())
            .collect_vec();

        let mut ideal_rlwe_sk = vec![0i32; bool_evaluator.pbs_info.rlwe_n()];
        parties.iter().for_each(|k| {
            izip!(ideal_rlwe_sk.iter_mut(), k.sk_rlwe.values()).for_each(|(ideal_i, s_i)| {
                *ideal_i = *ideal_i + s_i;
            });
        });

        println!("{:?}", &ideal_rlwe_sk);

        let mut m = true;
        for i in 0..100 {
            let pk_cr_seed = [0u8; 32];

            let public_key_share = parties
                .iter()
                .map(|k| bool_evaluator.multi_party_public_key_share(pk_cr_seed, k))
                .collect_vec();

            let collective_pk = PublicKey::<Vec<Vec<u64>>, DefaultSecureRng, ModularOpsU64>::from(
                public_key_share.as_slice(),
            );
            let lwe_ct = bool_evaluator.pk_encrypt(&collective_pk.key, m);

            let decryption_shares = parties
                .iter()
                .map(|k| bool_evaluator.multi_party_decryption_share(&lwe_ct, k))
                .collect_vec();

            let m_back = bool_evaluator.multi_party_decrypt(&decryption_shares, &lwe_ct);

            {
                let ideal_m = if m {
                    bool_evaluator.pbs_info.rlweq_by8
                } else {
                    bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                };
                let noise = measure_noise_lwe(
                    &lwe_ct,
                    &ideal_rlwe_sk,
                    &bool_evaluator.pbs_info.rlwe_modop,
                    &ideal_m,
                );
                println!("Noise: {noise}");
            }

            assert_eq!(m_back, m);
            m = !m;
        }
    }

    fn _collecitve_public_key_gen(rlwe_q: u64, parties_rlwe_sk: &[RlweSecret]) -> Vec<Vec<u64>> {
        let ring_size = parties_rlwe_sk[0].values.len();
        assert!(ring_size.is_power_of_two());
        let mut rng = DefaultSecureRng::new();
        let nttop = NttBackendU64::new(rlwe_q, ring_size);
        let modop = ModularOpsU64::new(rlwe_q);

        // Generate Pk shares
        let pk_seed = [0u8; 32];
        let pk_shares = parties_rlwe_sk.iter().map(|sk| {
            let mut p_rng = DefaultSecureRng::new_seeded(pk_seed);
            let mut share_out = vec![0u64; ring_size];
            public_key_share(
                &mut share_out,
                sk.values(),
                &modop,
                &nttop,
                &mut p_rng,
                &mut rng,
            );
            share_out
        });

        let mut pk_part_b = vec![0u64; ring_size];
        pk_shares.for_each(|share| modop.elwise_add_mut(&mut pk_part_b, &share));
        let mut pk_part_a = vec![0u64; ring_size];
        let mut p_rng = DefaultSecureRng::new_seeded(pk_seed);
        RandomUniformDist::random_fill(&mut p_rng, &rlwe_q, pk_part_a.as_mut_slice());

        vec![pk_part_a, pk_part_b]
    }

    fn _multi_party_all_keygen(
        bool_evaluator: &BoolEvaluator<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>,
        no_of_parties: usize,
    ) -> (
        Vec<ClientKey>,
        PublicKey<Vec<Vec<u64>>, DefaultSecureRng, ModularOpsU64>,
        Vec<
            CommonReferenceSeededMultiPartyServerKeyShare<
                Vec<Vec<u64>>,
                BoolParameters<u64>,
                [u8; 32],
            >,
        >,
        SeededMultiPartyServerKey<Vec<Vec<u64>>, [u8; 32], BoolParameters<u64>>,
        ServerKeyEvaluationDomain<Vec<Vec<u64>>, DefaultSecureRng, NttBackendU64>,
        ClientKey,
    ) {
        let parties = (0..no_of_parties)
            .map(|_| bool_evaluator.client_key())
            .collect_vec();

        let mut rng = DefaultSecureRng::new();

        // Collective public key
        let mut pk_cr_seed = [0u8; 32];
        rng.fill_bytes(&mut pk_cr_seed);
        let public_key_share = parties
            .iter()
            .map(|k| bool_evaluator.multi_party_public_key_share(pk_cr_seed, k))
            .collect_vec();
        let collective_pk = PublicKey::<Vec<Vec<u64>>, DefaultSecureRng, ModularOpsU64>::from(
            public_key_share.as_slice(),
        );

        // Server key
        let mut pbs_cr_seed = [1u8; 32];
        rng.fill_bytes(&mut pbs_cr_seed);
        let server_key_shares = parties
            .iter()
            .map(|k| {
                bool_evaluator.multi_party_server_key_share(pbs_cr_seed, &collective_pk.key, k)
            })
            .collect_vec();
        let seeded_server_key = aggregate_multi_party_server_key_shares::<
            _,
            _,
            (DefaultDecomposer<u64>, DefaultDecomposer<u64>),
            ModularOpsU64,
            NttBackendU64,
        >(&server_key_shares);
        let server_key_eval = ServerKeyEvaluationDomain::<_, DefaultSecureRng, NttBackendU64>::from(
            &seeded_server_key,
        );

        // construct ideal rlwe sk for meauring noise
        let ideal_client_key = {
            let mut ideal_rlwe_sk = vec![0i32; bool_evaluator.pbs_info.rlwe_n()];
            parties.iter().for_each(|k| {
                izip!(ideal_rlwe_sk.iter_mut(), k.sk_rlwe.values()).for_each(|(ideal_i, s_i)| {
                    *ideal_i = *ideal_i + s_i;
                });
            });
            let mut ideal_lwe_sk = vec![0i32; bool_evaluator.pbs_info.lwe_n()];
            parties.iter().for_each(|k| {
                izip!(ideal_lwe_sk.iter_mut(), k.sk_lwe.values()).for_each(|(ideal_i, s_i)| {
                    *ideal_i = *ideal_i + s_i;
                });
            });

            ClientKey {
                sk_lwe: LweSecret {
                    values: ideal_lwe_sk,
                },
                sk_rlwe: RlweSecret {
                    values: ideal_rlwe_sk,
                },
            }
        };

        (
            parties,
            collective_pk,
            server_key_shares,
            seeded_server_key,
            server_key_eval,
            ideal_client_key,
        )
    }

    #[test]
    fn multi_party_nand() {
        let mut bool_evaluator =
            BoolEvaluator::<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>::new(MP_BOOL_PARAMS);

        let (parties, collective_pk, _, _, server_key_eval, ideal_client_key) =
            _multi_party_all_keygen(&bool_evaluator, 2);

        let mut m0 = true;
        let mut m1 = false;

        let mut lwe0 = bool_evaluator.pk_encrypt(&collective_pk.key, m0);
        let mut lwe1 = bool_evaluator.pk_encrypt(&collective_pk.key, m1);

        for _ in 0..2000 {
            let lwe_out = bool_evaluator.nand(&lwe0, &lwe1, &server_key_eval);

            let m_expected = !(m0 & m1);

            // measure noise
            {
                let noise0 = {
                    let ideal = if m0 {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &lwe0,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal,
                    );
                    let v = decrypt_lwe(
                        &lwe0,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };
                let noise1 = {
                    let ideal = if m1 {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.rlwe_q() - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &lwe1,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal,
                    );
                    let v = decrypt_lwe(
                        &lwe1,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };

                // Trace PBS
                PBSTracer::with_local(|t| {
                    t.trace(
                        &MP_BOOL_PARAMS,
                        &ideal_client_key.sk_lwe.values(),
                        &ideal_client_key.sk_rlwe.values(),
                    )
                });

                let noise_out = {
                    let ideal_m = if m_expected {
                        bool_evaluator.pbs_info.rlweq_by8
                    } else {
                        bool_evaluator.pbs_info.parameters.rlwe_q().0
                            - bool_evaluator.pbs_info.rlweq_by8
                    };
                    let n = measure_noise_lwe(
                        &lwe_out,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                        &ideal_m,
                    );
                    let v = decrypt_lwe(
                        &lwe_out,
                        ideal_client_key.sk_rlwe.values(),
                        &bool_evaluator.pbs_info.rlwe_modop,
                    );
                    (n, v)
                };
                dbg!(m0, m1, m_expected);
                println!(
                    "ct0 (noise, message): {:?}  \n ct1 (noise, message): {:?} \n PBS (noise, message): {:?}", noise0, noise1, noise_out
                );
            }

            // multi-party decrypt
            let decryption_shares = parties
                .iter()
                .map(|k| bool_evaluator.multi_party_decryption_share(&lwe_out, k))
                .collect_vec();
            let m_back = bool_evaluator.multi_party_decrypt(&decryption_shares, &lwe_out);

            // let m_back = bool_evaluator.sk_decrypt(&lwe_out, &ideal_client_key);

            assert!(m_expected == m_back, "Expected {m_expected}, got {m_back}");
            m1 = m0;
            m0 = m_expected;

            lwe1 = lwe0;
            lwe0 = lwe_out;
        }
    }

    #[test]
    fn tester() {
        // pub(super) const TEST_MP_BOOL_PARAMS: BoolParameters<u64> =
        // BoolParameters::<u64> {     rlwe_q: 1152921504606830593,
        //     rlwe_logq: 60,
        //     lwe_q: 1 << 20,
        //     lwe_logq: 20,
        //     br_q: 1 << 11,
        //     rlwe_n: 1 << 11,
        //     lwe_n: 500,
        //     d_rgsw: 4,
        //     logb_rgsw: 12,
        //     d_lwe: 5,
        //     logb_lwe: 4,
        //     g: 5,
        //     w: 1,
        // };

        let bool_evaluator =
            BoolEvaluator::<Vec<Vec<u64>>, NttBackendU64, ModularOpsU64>::new(MP_BOOL_PARAMS);

        // let (_, collective_pk, _, _, server_key_eval, ideal_client_key) =
        //     _multi_party_all_keygen(&bool_evaluator, 20);
        let no_of_parties = 2;
        let lwe_q = bool_evaluator.pbs_info.parameters.lwe_q().0;
        let rlwe_q = bool_evaluator.pbs_info.parameters.rlwe_q().0;
        let lwe_n = bool_evaluator.pbs_info.parameters.lwe_n().0;
        let rlwe_n = bool_evaluator.pbs_info.parameters.rlwe_n().0;
        let lwe_modop = &bool_evaluator.pbs_info.lwe_modop;
        let rlwe_nttop = &bool_evaluator.pbs_info.rlwe_nttop;
        let rlwe_modop = &bool_evaluator.pbs_info.rlwe_modop;

        let rlwe_rgsw_decomposer = &bool_evaluator.pbs_info.rlwe_rgsw_decomposer;
        let rlwe_rgsw_gadget_a = rlwe_rgsw_decomposer.0.gadget_vector();
        let rlwe_rgsw_gadget_b = rlwe_rgsw_decomposer.1.gadget_vector();

        // let rgsw_rgsw_decomposer = &bool_evaluator
        //     .pbs_info
        //     .parameters
        //     .rgsw_rgsw_decomposer::<DefaultDecomposer<u64>>();
        // let rgsw_rgsw_gagdet_a = rgsw_rgsw_decomposer.a().gadget_vector();
        // let rgsw_rgsw_gagdet_b = rgsw_rgsw_decomposer.b().gadget_vector();

        let parties = (0..no_of_parties)
            .map(|_| bool_evaluator.client_key())
            .collect_vec();

        let ideal_client_key = {
            let mut ideal_rlwe_sk = vec![0i32; bool_evaluator.pbs_info.rlwe_n()];
            parties.iter().for_each(|k| {
                izip!(ideal_rlwe_sk.iter_mut(), k.sk_rlwe.values()).for_each(|(ideal_i, s_i)| {
                    *ideal_i = *ideal_i + s_i;
                });
            });
            let mut ideal_lwe_sk = vec![0i32; bool_evaluator.pbs_info.lwe_n()];
            parties.iter().for_each(|k| {
                izip!(ideal_lwe_sk.iter_mut(), k.sk_lwe.values()).for_each(|(ideal_i, s_i)| {
                    *ideal_i = *ideal_i + s_i;
                });
            });

            ClientKey {
                sk_lwe: LweSecret {
                    values: ideal_lwe_sk,
                },
                sk_rlwe: RlweSecret {
                    values: ideal_rlwe_sk,
                },
            }
        };

        // check noise in freshly encrypted RLWE ciphertext (ie var_fresh)
        if false {
            let mut rng = DefaultSecureRng::new();
            let mut check = Stats { samples: vec![] };
            for _ in 0..10 {
                // generate a new collective public key
                let mut pk_cr_seed = [0u8; 32];
                rng.fill_bytes(&mut pk_cr_seed);
                let public_key_share = parties
                    .iter()
                    .map(|k| bool_evaluator.multi_party_public_key_share(pk_cr_seed, k))
                    .collect_vec();
                let collective_pk =
                    PublicKey::<Vec<Vec<u64>>, DefaultSecureRng, ModularOpsU64>::from(
                        public_key_share.as_slice(),
                    );

                let mut m = vec![0u64; rlwe_n];
                RandomUniformDist::random_fill(&mut rng, &rlwe_q, m.as_mut_slice());
                let mut rlwe_ct = vec![vec![0u64; rlwe_n]; 2];
                public_key_encrypt_rlwe(
                    &mut rlwe_ct,
                    &collective_pk.key,
                    &m,
                    rlwe_modop,
                    rlwe_nttop,
                    &mut rng,
                );

                let mut m_back = vec![0u64; rlwe_n];
                decrypt_rlwe(
                    &rlwe_ct,
                    ideal_client_key.sk_rlwe.values(),
                    &mut m_back,
                    rlwe_nttop,
                    rlwe_modop,
                );

                rlwe_modop.elwise_sub_mut(m_back.as_mut_slice(), m.as_slice());

                check.add_more(Vec::<i64>::try_convert_from(&m_back, &rlwe_q).as_slice());
            }

            println!("Public key Std: {}", check.std_dev().abs().log2());
        }

        if true {
            // Generate server key shares
            let mut rng = DefaultSecureRng::new();
            let mut pk_cr_seed = [0u8; 32];
            rng.fill_bytes(&mut pk_cr_seed);
            let public_key_share = parties
                .iter()
                .map(|k| bool_evaluator.multi_party_public_key_share(pk_cr_seed, k))
                .collect_vec();
            let collective_pk = PublicKey::<Vec<Vec<u64>>, DefaultSecureRng, ModularOpsU64>::from(
                public_key_share.as_slice(),
            );
            let pbs_cr_seed = [0u8; 32];
            rng.fill_bytes(&mut pk_cr_seed);
            let server_key_shares = parties
                .iter()
                .map(|k| {
                    bool_evaluator.multi_party_server_key_share(pbs_cr_seed, &collective_pk.key, k)
                })
                .collect_vec();

            let seeded_server_key = aggregate_multi_party_server_key_shares::<
                _,
                _,
                (DefaultDecomposer<u64>, DefaultDecomposer<u64>),
                ModularOpsU64,
                NttBackendU64,
            >(&server_key_shares);

            // Check noise in RGSW ciphertexts of ideal LWE secret elements
            if true {
                let mut check = Stats { samples: vec![] };
                izip!(
                    ideal_client_key.sk_lwe.values.iter(),
                    seeded_server_key.rgsw_cts.iter()
                )
                .for_each(|(s_i, rgsw_ct_i)| {
                    // X^{s[i]}
                    let mut m_si = vec![0u64; rlwe_n];
                    let s_i = *s_i * (bool_evaluator.pbs_info.embedding_factor as i32);
                    if s_i < 0 {
                        m_si[rlwe_n - (s_i.abs() as usize)] = rlwe_q - 1;
                    } else {
                        m_si[s_i as usize] = 1;
                    }

                    // RLWE(-sm)
                    let mut neg_s_eval =
                        Vec::<u64>::try_convert_from(ideal_client_key.sk_rlwe.values(), &rlwe_q);
                    rlwe_modop.elwise_neg_mut(&mut neg_s_eval);
                    rlwe_nttop.forward(&mut neg_s_eval);
                    for j in 0..rlwe_rgsw_decomposer.a().decomposition_count() {
                        // -s[X]*X^{s_lwe[i]}*B_j
                        let mut m_ideal = m_si.clone();
                        rlwe_nttop.forward(m_ideal.as_mut_slice());
                        rlwe_modop.elwise_mul_mut(m_ideal.as_mut_slice(), neg_s_eval.as_slice());
                        rlwe_nttop.backward(m_ideal.as_mut_slice());
                        rlwe_modop
                            .elwise_scalar_mul_mut(m_ideal.as_mut_slice(), &rlwe_rgsw_gadget_a[j]);

                        // RLWE(-s*X^{s_lwe[i]}*B_j)
                        let mut rlwe_ct = vec![vec![0u64; rlwe_n]; 2];
                        rlwe_ct[0].copy_from_slice(&rgsw_ct_i[j]);
                        rlwe_ct[1].copy_from_slice(
                            &rgsw_ct_i[j + rlwe_rgsw_decomposer.a().decomposition_count()],
                        );

                        let mut m_back = vec![0u64; rlwe_n];
                        decrypt_rlwe(
                            &rlwe_ct,
                            ideal_client_key.sk_rlwe.values(),
                            &mut m_back,
                            rlwe_nttop,
                            rlwe_modop,
                        );

                        // diff
                        rlwe_modop.elwise_sub_mut(&mut m_back, &m_ideal);
                        check.add_more(&Vec::<i64>::try_convert_from(&m_back, &rlwe_q));
                    }

                    // RLWE'(m)
                    for j in 0..rlwe_rgsw_decomposer.b().decomposition_count() {
                        // X^{s_lwe[i]}*B_j
                        let mut m_ideal = m_si.clone();
                        rlwe_modop
                            .elwise_scalar_mul_mut(m_ideal.as_mut_slice(), &rlwe_rgsw_gadget_b[j]);

                        // RLWE(X^{s_lwe[i]}*B_j)
                        let mut rlwe_ct = vec![vec![0u64; rlwe_n]; 2];
                        rlwe_ct[0].copy_from_slice(
                            &rgsw_ct_i[j + (2 * rlwe_rgsw_decomposer.a().decomposition_count())],
                        );
                        rlwe_ct[1].copy_from_slice(
                            &rgsw_ct_i[j
                                + (2 * rlwe_rgsw_decomposer.a().decomposition_count()
                                    + rlwe_rgsw_decomposer.b().decomposition_count())],
                        );

                        let mut m_back = vec![0u64; rlwe_n];
                        decrypt_rlwe(
                            &rlwe_ct,
                            ideal_client_key.sk_rlwe.values(),
                            &mut m_back,
                            rlwe_nttop,
                            rlwe_modop,
                        );

                        // diff
                        rlwe_modop.elwise_sub_mut(&mut m_back, &m_ideal);
                        check.add_more(&Vec::<i64>::try_convert_from(&m_back, &rlwe_q));
                    }
                });
                println!(
                    "RGSW Std: {} {} ;; max={}",
                    check.mean(),
                    check.std_dev().abs().log2(),
                    check.samples.iter().max().unwrap()
                );
            }

            // check noise in RLWE x RGSW(X^{s_i}) where RGSW is accunulated RGSW ciphertext
            if false {
                let mut check = Stats { samples: vec![] };
                // server key in Evaluation domain
                let server_key_eval_domain =
                    ServerKeyEvaluationDomain::<_, DefaultSecureRng, NttBackendU64>::from(
                        &seeded_server_key,
                    );
                izip!(
                    ideal_client_key.sk_lwe.values(),
                    seeded_server_key.rgsw_cts.iter()
                )
                .for_each(|(s_i, rgsw_ct_i)| {
                    let mut rgsw_ct_i = rgsw_ct_i.clone();
                    rgsw_ct_i
                        .iter_mut()
                        .for_each(|ri| rlwe_nttop.forward(ri.as_mut()));

                    let mut m = vec![0u64; rlwe_n];
                    RandomUniformDist::random_fill(&mut rng, &rlwe_q, m.as_mut_slice());
                    let mut rlwe_ct = vec![vec![0u64; rlwe_n]; 2];
                    public_key_encrypt_rlwe(
                        &mut rlwe_ct,
                        &collective_pk.key,
                        &m,
                        rlwe_modop,
                        rlwe_nttop,
                        &mut rng,
                    );

                    // RLWE(m*X^{s[i]}) = RLWE(m) x RGSW(X^{s[i]})
                    let mut rlwe_after = RlweCiphertext::<_, DefaultSecureRng>::new_trivial(vec![
                        vec![0u64; rlwe_n],
                        m.clone(),
                    ]);
                    // let mut rlwe_after =
                    //     RlweCiphertext::<_, DefaultSecureRng>::from_raw(rlwe_ct.clone(), false);
                    let mut scratch = vec![
                        vec![0u64; rlwe_n];
                        std::cmp::max(
                            rlwe_rgsw_decomposer.0.decomposition_count(),
                            rlwe_rgsw_decomposer.1.decomposition_count()
                        ) + 2
                    ];
                    rlwe_by_rgsw(
                        &mut rlwe_after,
                        &rgsw_ct_i,
                        &mut scratch,
                        rlwe_rgsw_decomposer,
                        rlwe_nttop,
                        rlwe_modop,
                    );

                    // m1 = X^{s[i]}
                    let mut m1 = vec![0u64; rlwe_n];
                    let s_i = *s_i * (bool_evaluator.pbs_info.embedding_factor as i32);
                    if s_i < 0 {
                        m1[rlwe_n - (s_i.abs() as usize)] = rlwe_q - 1;
                    } else {
                        m1[s_i as usize] = 1;
                    }

                    // (m+e) * m1
                    let mut m_plus_e_times_m1 = m.clone();
                    // decrypt_rlwe(
                    //     &rlwe_ct,
                    //     ideal_client_key.sk_rlwe.values(),
                    //     &mut m_plus_e_times_m1,
                    //     rlwe_nttop,
                    //     rlwe_modop,
                    // );
                    rlwe_nttop.forward(m_plus_e_times_m1.as_mut_slice());
                    rlwe_nttop.forward(m1.as_mut_slice());
                    rlwe_modop.elwise_mul_mut(m_plus_e_times_m1.as_mut_slice(), m1.as_slice());
                    rlwe_nttop.backward(m_plus_e_times_m1.as_mut_slice());

                    // Resulting RLWE ciphertext will equal: (m0m1 + em1) + e_{rlsw x rgsw}.
                    // Hence, resulting rlwe ciphertext will have error em1 + e_{rlwe x rgsw}.
                    // Here we're only concerned with e_{rlwe x rgsw}, that is noise caused due to
                    // RLWExRGSW. Also note, in practice m1 is a monomial, for ex, X^{s_{i}}, for
                    // some i and var(em1) = var(e).
                    let mut m_plus_e_times_m1_more_e = vec![0u64; rlwe_n];
                    decrypt_rlwe(
                        &rlwe_after,
                        ideal_client_key.sk_rlwe.values(),
                        &mut m_plus_e_times_m1_more_e,
                        rlwe_nttop,
                        rlwe_modop,
                    );

                    // diff
                    rlwe_modop.elwise_sub_mut(
                        m_plus_e_times_m1_more_e.as_mut_slice(),
                        m_plus_e_times_m1.as_slice(),
                    );

                    let noise = measure_noise(
                        &rlwe_after,
                        &m_plus_e_times_m1,
                        rlwe_nttop,
                        rlwe_modop,
                        ideal_client_key.sk_rlwe.values(),
                    );
                    print!("NOISE: {}", noise);

                    check.add_more(&Vec::<i64>::try_convert_from(
                        &m_plus_e_times_m1_more_e,
                        &rlwe_q,
                    ));
                });
                println!(
                    "RLWE x RGSW, where RGSW has noise var_brk, std: {} {}",
                    check.std_dev(),
                    check.std_dev().abs().log2()
                )
            }
        }

        // Check noise in fresh RGSW ciphertexts, ie X^{s_j[i]}, must equal noise in
        // fresh RLWE ciphertext
        if true {}
        // test LWE ksk from RLWE -> LWE
        // if false {
        //     let logp = 2;
        //     let mut rng = DefaultSecureRng::new();

        //     let m = 1;
        //     let encoded_m = m << (lwe_logq - logp);

        //     // Encrypt
        //     let mut lwe_ct = vec![0u64; rlwe_n + 1];
        //     encrypt_lwe(
        //         &mut lwe_ct,
        //         &encoded_m,
        //         ideal_client_key.sk_rlwe.values(),
        //         lwe_modop,
        //         &mut rng,
        //     );

        //     // key switch
        //     let lwe_decomposer = &bool_evaluator.decomposer_lwe;
        //     let mut lwe_out = vec![0u64; lwe_n + 1];
        //     lwe_key_switch(
        //         &mut lwe_out,
        //         &lwe_ct,
        //         &server_key_eval.lwe_ksk,
        //         lwe_modop,
        //         lwe_decomposer,
        //     );

        //     let encoded_m_back = decrypt_lwe(&lwe_out,
        // ideal_client_key.sk_lwe.values(), lwe_modop);     let m_back
        // =         ((encoded_m_back as f64 * (1 << logp) as f64) /
        // (lwe_q as f64)).round() as u64;     dbg!(m_back, m);

        //     let noise = measure_noise_lwe(
        //         &lwe_out,
        //         ideal_client_key.sk_lwe.values(),
        //         lwe_modop,
        //         &encoded_m,
        //     );

        //     println!("Noise: {noise}");
        // }

        // Measure noise in RGSW ciphertexts of ideal LWE secrets
        // if true {
        //     let gadget_vec = gadget_vector(
        //         bool_evaluator.parameters.rlwe_logq,
        //         bool_evaluator.parameters.logb_rgsw,
        //         bool_evaluator.parameters.d_rgsw,
        //     );

        //     for i in 0..20 {
        //         // measure noise in RGSW(s[i])
        //         let si =
        //             ideal_client_key.sk_lwe.values[i] *
        // (bool_evaluator.embedding_factor as i32);         let mut
        // si_poly = vec![0u64; rlwe_n];         if si < 0 {
        //             si_poly[rlwe_n - (si.abs() as usize)] = rlwe_q - 1;
        //         } else {
        //             si_poly[(si.abs() as usize)] = 1;
        //         }

        //         let mut rgsw_si = server_key_eval.rgsw_cts[i].clone();
        //         rgsw_si
        //             .iter_mut()
        //             .for_each(|ri| rlwe_nttop.backward(ri.as_mut()));

        //         println!("####### Noise in RGSW(X^s_{i}) #######");
        //         _measure_noise_rgsw(
        //             &rgsw_si,
        //             &si_poly,
        //             ideal_client_key.sk_rlwe.values(),
        //             &gadget_vec,
        //             rlwe_q,
        //         );
        //         println!("####### ##################### #######");
        //     }
        // }

        // // measure noise grwoth in RLWExRGSW
        // if true {
        //     let mut rng = DefaultSecureRng::new();
        //     let mut carry_m = vec![0u64; rlwe_n];
        //     RandomUniformDist::random_fill(&mut rng, &rlwe_q,
        // carry_m.as_mut_slice());

        //     // RGSW(carrym)
        //     let trivial_rlwect = vec![vec![0u64; rlwe_n], carry_m.clone()];
        //     let mut rlwe_ct = RlweCiphertext::<_,
        // DefaultSecureRng>::from_raw(trivial_rlwect, true);

        //     let mut scratch_matrix_dplus2_ring = vec![vec![0u64; rlwe_n];
        // d_rgsw + 2];     let mul_mod =
        //         |v0: &u64, v1: &u64| (((*v0 as u128 * *v1 as u128) % (rlwe_q as u128)) as u64);

        //     for i in 0..bool_evaluator.parameters.lwe_n {
        //         rlwe_by_rgsw(
        //             &mut rlwe_ct,
        //             server_key_eval.rgsw_ct_lwe_si(i),
        //             &mut scratch_matrix_dplus2_ring,
        //             rlwe_decomposer,
        //             rlwe_nttop,
        //             rlwe_modop,
        //         );

        //         // carry_m[X] * s_i[X]
        //         let si =
        //             ideal_client_key.sk_lwe.values[i] *
        // (bool_evaluator.embedding_factor as i32);         let mut
        // si_poly = vec![0u64; rlwe_n];         if si < 0 {
        //             si_poly[rlwe_n - (si.abs() as usize)] = rlwe_q - 1;
        //         } else {
        //             si_poly[(si.abs() as usize)] = 1;
        //         }
        //         carry_m = negacyclic_mul(&carry_m, &si_poly, mul_mod,
        // rlwe_q);

        //         let noise = measure_noise(
        //             &rlwe_ct,
        //             &carry_m,
        //             rlwe_nttop,
        //             rlwe_modop,
        //             ideal_client_key.sk_rlwe.values(),
        //         );
        //         println!("Noise RLWE(carry_m) accumulating {i}^th secret
        // monomial: {noise}");     }
        // }

        // // Check galois keys
        // if false {
        //     let g = bool_evaluator.g() as isize;
        //     let mut rng = DefaultSecureRng::new();
        //     let mut scratch_matrix_dplus2_ring = vec![vec![0u64; rlwe_n];
        // d_rgsw + 2];     for i in [g, -g] {
        //         let mut m = vec![0u64; rlwe_n];
        //         RandomUniformDist::random_fill(&mut rng, &rlwe_q,
        // m.as_mut_slice());         let mut rlwe_ct = {
        //             let mut data = vec![vec![0u64; rlwe_n]; 2];
        //             public_key_encrypt_rlwe(
        //                 &mut data,
        //                 &collective_pk.key,
        //                 &m,
        //                 rlwe_modop,
        //                 rlwe_nttop,
        //                 &mut rng,
        //             );
        //             RlweCiphertext::<_, DefaultSecureRng>::from_raw(data,
        // false)         };

        //         let auto_key = server_key_eval.galois_key_for_auto(i);
        //         let (auto_map_index, auto_map_sign) =
        // generate_auto_map(rlwe_n, i);         galois_auto(
        //             &mut rlwe_ct,
        //             auto_key,
        //             &mut scratch_matrix_dplus2_ring,
        //             &auto_map_index,
        //             &auto_map_sign,
        //             rlwe_modop,
        //             rlwe_nttop,
        //             rlwe_decomposer,
        //         );

        //         // send m(X) -> m(X^i)
        //         let mut m_k = vec![0u64; rlwe_n];
        //         izip!(m.iter(), auto_map_index.iter(),
        // auto_map_sign.iter()).for_each(             |(mi, to_index, to_sign)|
        // {                 if !to_sign {
        //                     m_k[*to_index] = rlwe_q - *mi;
        //                 } else {
        //                     m_k[*to_index] = *mi;
        //                 }
        //             },
        //         );

        //         // measure noise
        //         let noise = measure_noise(
        //             &rlwe_ct,
        //             &m_k,
        //             rlwe_nttop,
        //             rlwe_modop,
        //             ideal_client_key.sk_rlwe.values(),
        //         );

        //         println!("Noise after auto k={i}: {noise}");
        //     }
        // }
    }
}