// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

pub use air::{
    errors::VerifierError, proof::StarkProof, Air, FieldExtension, HashFunction, TraceInfo,
};
pub use utils::{ByteWriter, Serializable};

pub use crypto;
use crypto::{
    hashers::{Blake3_256, Sha3_256},
    ElementHasher, RandomCoin,
};
use fri::FriVerifier;

pub use math;
use math::{FieldElement, StarkField};

mod channel;
use channel::VerifierChannel;

mod evaluator;
use evaluator::evaluate_constraints;

mod composer;
use composer::DeepComposer;

// VERIFIER
// ================================================================================================
/// Verifies STARK `proof` attesting that the computation specified by `AIR` was executed correctly
/// against the provided `pub_inputs`.
#[rustfmt::skip]
pub fn verify<AIR: Air>(
    proof: StarkProof,
    pub_inputs: AIR::PublicInputs,
) -> Result<(), VerifierError> {
    // build a seed for the public coin; the initial seed is the hash of public inputs and proof
    // context, but as the protocol progresses, the coin will be reseeded with the info received
    // from the prover
    let mut public_coin_seed = Vec::new();
    pub_inputs.write_into(&mut public_coin_seed);
    proof.context.write_into(&mut public_coin_seed);

    // create AIR instance for the computation specified in the proof
    let air = AIR::new(proof.get_trace_info(), pub_inputs, proof.options().clone());

    // figure out which version of the generic proof verification procedure to run. this is a sort
    // of static dispatch for selecting two generic parameter: extension field and hash function.
    match air.options().field_extension() {
        FieldExtension::None => match air.options().hash_fn() {
            HashFunction::Blake3_256 => {
                let public_coin = RandomCoin::new(&public_coin_seed);
                let channel = VerifierChannel::new(&air, proof)?;
                perform_verification::
                    <AIR, AIR::BaseElement, Blake3_256<AIR::BaseElement>>
                    (air, channel, public_coin)
            }
            HashFunction::Sha3_256 => {
                let public_coin = RandomCoin::new(&public_coin_seed);
                let channel = VerifierChannel::new(&air, proof)?;
                perform_verification::
                    <AIR, AIR::BaseElement, Sha3_256<AIR::BaseElement>>
                    (air, channel, public_coin)
            }
        },
        FieldExtension::Quadratic => match air.options().hash_fn() {
            HashFunction::Blake3_256 => {
                let public_coin = RandomCoin::new(&public_coin_seed);
                let channel = VerifierChannel::new(&air, proof)?;
                perform_verification::
                    <AIR, <AIR::BaseElement as StarkField>::QuadExtension, Blake3_256<AIR::BaseElement>>
                    (air, channel, public_coin)
            }
            HashFunction::Sha3_256 => {
                let public_coin = RandomCoin::new(&public_coin_seed);
                let channel = VerifierChannel::new(&air, proof)?;
                perform_verification::
                    <AIR, <AIR::BaseElement as StarkField>::QuadExtension, Sha3_256<AIR::BaseElement>>
                    (air, channel, public_coin)
            }
        },
    }
}

// VERIFICATION PROCEDURE
// ================================================================================================
/// Performs the actual verification by reading the data from the `channel` and making sure it
/// attests to a correct execution of the computation specified by the provided `air`.
fn perform_verification<A, E, H>(
    air: A,
    mut channel: VerifierChannel<A::BaseElement, E, H>,
    mut public_coin: RandomCoin<A::BaseElement, H>,
) -> Result<(), VerifierError>
where
    A: Air,
    E: FieldElement<BaseField = A::BaseElement>,
    H: ElementHasher<BaseField = A::BaseElement>,
{
    // 1 ----- trace commitment -------------------------------------------------------------------
    // read the commitment to evaluations of the trace polynomials over the LDE domain sent by the
    // prover, use it to update the public coin, and draw a set of random coefficients from the
    // coin; in the interactive version of the protocol, the verifier sends these coefficients to
    // the prover, and prover uses them to compute constraint composition polynomial.
    let trace_commitment = channel.read_trace_commitment();
    public_coin.reseed(trace_commitment);
    let constraint_coeffs = air
        .get_constraint_composition_coefficients(&mut public_coin)
        .map_err(|_| VerifierError::RandomCoinError)?;

    // 2 ----- constraint commitment --------------------------------------------------------------
    // read the commitment to evaluations of the constraint composition polynomial over the LDE
    // domain sent by the prover, use it to update the public coin, and draw an out-of-domain point
    // z from the coin; in the interactive version of the protocol, the verifier sends this point z
    // to the prover, and the prover evaluates trace and constraint composition polynomials at z,
    // and send the results back to the verifier.
    let constraint_commitment = channel.read_constraint_commitment();
    public_coin.reseed(constraint_commitment);
    let z = public_coin
        .draw::<E>()
        .map_err(|_| VerifierError::RandomCoinError)?;

    // 3 ----- OOD consistency check --------------------------------------------------------------
    // make sure that evaluations obtained by evaluating constraints over the out-of-domain frame
    // are consistent with the evaluations of composition polynomial columns sent by the prover

    // read the out-of-domain evaluation frame sent by the prover and evaluate constraints over it;
    // also, reseed the public coin with the OOD frame received from the prover
    let ood_frame = channel.read_ood_evaluation_frame();
    let ood_constraint_evaluation_1 = evaluate_constraints(&air, constraint_coeffs, &ood_frame, z);
    public_coin.reseed(H::hash_elements(ood_frame.current()));
    public_coin.reseed(H::hash_elements(ood_frame.next()));

    // read evaluations of composition polynomial columns sent by the prover, and reduce them into
    // a single value by computing sum(z^i * value_i), where value_i is the evaluation of the ith
    // column polynomial at z^m, where m is the total number of column polynomials; also, reseed
    // the public coin with the OOD constraint evaluations received from the prover.
    let ood_evaluations = channel.read_ood_evaluations();
    let ood_constraint_evaluation_2 = ood_evaluations
        .iter()
        .enumerate()
        .fold(E::ZERO, |result, (i, &value)| {
            result + z.exp((i as u32).into()) * value
        });
    public_coin.reseed(H::hash_elements(&ood_evaluations));

    // finally, make sure the values are the same
    if ood_constraint_evaluation_1 != ood_constraint_evaluation_2 {
        return Err(VerifierError::InconsistentOodConstraintEvaluations);
    }

    // 4 ----- FRI commitments --------------------------------------------------------------------
    // draw coefficients for computing DEEP composition polynomial from the public coin; in the
    // interactive version of the protocol, the verifier sends these coefficients to the prover
    // and the prover uses them to compute the DEEP composition polynomial. the prover, then
    // applies FRI protocol to the evaluations of the DEEP composition polynomial.
    let deep_coefficients = air
        .get_deep_composition_coefficients::<E, H>(&mut public_coin)
        .map_err(|_| VerifierError::RandomCoinError)?;

    // instantiates a FRI verifier with the FRI layer commitments read from the channel. From the
    // verifier's perspective, this is equivalent to executing the commit phase of the FRI protocol.
    // The verifier uses these commitments to update the public coin and draw random points alpha
    // from them; in the interactive version of the protocol, the verifier sends these alphas to
    // the prover, and the prover uses them to compute and commit to the subsequent FRI layers.
    let fri_verifier = FriVerifier::new(
        &mut channel,
        &mut public_coin,
        air.options().to_fri_options(),
        air.trace_poly_degree(),
    )
    .map_err(VerifierError::FriVerificationFailed)?;
    // TODO: make sure air.lde_domain_size() == fri_verifier.domain_size()

    // 5 ----- trace and constraint queries -------------------------------------------------------
    // read proof-of-work nonce sent by the prover and update the public coin with it
    let pow_nonce = channel.read_pow_nonce();
    public_coin.reseed_with_int(pow_nonce);

    // make sure the proof-of-work specified by the grinding factor is satisfied
    if public_coin.leading_zeros() < air.options().grinding_factor() {
        return Err(VerifierError::QuerySeedProofOfWorkVerificationFailed);
    }

    // draw pseudo-random query positions for the LDE domain from the public coin; in the
    // interactive version of the protocol, the verifier sends these query positions to the prover,
    // and the prover responds with decommitments against these positions for trace and constraint
    // composition polynomial evaluations.
    let query_positions = public_coin
        .draw_integers(air.options().num_queries(), air.lde_domain_size())
        .map_err(|_| VerifierError::RandomCoinError)?;

    // read evaluations of trace and constraint composition polynomials at the queried positions;
    // this also checks that the read values are valid against trace and constraint commitments
    let queried_trace_states = channel.read_trace_states(&query_positions, &trace_commitment)?;
    let queried_evaluations =
        channel.read_constraint_evaluations(&query_positions, &constraint_commitment)?;

    // 6 ----- DEEP composition -------------------------------------------------------------------
    // compute evaluations of the DEEP composition polynomial at the queried positions
    let composer = DeepComposer::new(&air, &query_positions, z, deep_coefficients);
    let t_composition = composer.compose_registers(queried_trace_states, ood_frame);
    let c_composition = composer.compose_constraints(queried_evaluations, ood_evaluations);
    let deep_evaluations = composer.combine_compositions(t_composition, c_composition);

    // 7 ----- Verify low-degree proof -------------------------------------------------------------
    // make sure that evaluations of the DEEP composition polynomial we computed in the previous
    // step are in fact evaluations of a polynomial of degree equal to trace polynomial degree
    fri_verifier
        .verify(&mut channel, &deep_evaluations, &query_positions)
        .map_err(VerifierError::FriVerificationFailed)
}
