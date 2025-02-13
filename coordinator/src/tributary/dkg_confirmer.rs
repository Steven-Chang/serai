use std::collections::HashMap;

use zeroize::Zeroizing;

use rand_core::SeedableRng;
use rand_chacha::ChaCha20Rng;

use transcript::{Transcript, RecommendedTranscript};
use ciphersuite::{Ciphersuite, Ristretto};
use frost::{
  FrostError,
  dkg::{Participant, musig::musig},
  sign::*,
};
use frost_schnorrkel::Schnorrkel;

use serai_client::validator_sets::primitives::{KeyPair, musig_context, set_keys_message};

use crate::tributary::TributarySpec;

/*
  The following confirms the results of the DKG performed by the Processors onto Substrate.

  This is done by a signature over the generated key pair by the validators' MuSig-aggregated
  public key. The MuSig-aggregation achieves on-chain efficiency and prevents on-chain censorship
  of individual validator's DKG results by the Serai validator set.

  Since we're using the validators public keys, as needed for their being the root of trust, the
  coordinator must perform the signing. This is distinct from all other group-signing operations
  which are generally done by the processor.

  Instead of maintaining state, the following rebuilds the full state on every call. This is deemed
  acceptable re: performance as:

  1) The DKG confirmation is only done upon the start of the Tributary.
  2) This is an O(n) algorithm.
  3) The size of the validator set is bounded by MAX_KEY_SHARES_PER_SET.

  Accordingly, this should be infrequently ran and of tolerable algorithmic complexity.

  As for safety, it is explicitly unsafe to reuse nonces across signing sessions. This is in
  contradiction with our rebuilding which is dependent on deterministic nonces. Safety is derived
  from the deterministic nonces being context-bound under a BFT protocol. The flow is as follows:

  1) Derive a deterministic nonce by hashing the private key, Tributary parameters, and attempt.
  2) Publish the nonces' commitments, receiving everyone elses *and the DKG shares determining the
     message to be signed*.
  3) Sign and publish the signature share.

  In order for nonce re-use to occur, the received nonce commitments, or the received DKG shares,
  would have to be distinct and sign would have to be called again.

  Before we act on any received messages, they're ordered and finalized by a BFT algorithm. The
  only way to operate on distinct received messages would be if:

  1) A logical flaw exists, letting new messages over write prior messages
  2) A reorganization occured from chain A to chain B, and with it, different messages

  Reorganizations are not supported, as BFT is assumed by the presence of a BFT algorithm. While
  a significant amount of processes may be byzantine, leading to BFT being broken, that still will
  not trigger a reorganization. The only way to move to a distinct chain, with distinct messages,
  would be by rebuilding the local process entirely (this time following chain B).

  Accordingly, safety follows if:

  1) The local view of received messages is static
  2) The local process doesn't rebuild after a byzantine fault produces multiple blockchains

  We assume the former. The latter is deemed acceptable but sub-optimal.

  The benefit for this behavior is that on a validator's infrastructure collapsing, they can
  successfully rebuild on a new system.

  TODO: Replace this with entropy. If a validator happens to have their infrastructure fail at this
  exact moment, they should just be kicked out and accept the loss. The risk of losing a private
  key on rebuild, by a feature meant to enable rebuild, can't be successfully argued for.

  Not only do we need to use randomly selected entropy, we need to confirm our local preprocess
  matches the on-chain preprocess before actually publishing our shares.

  We also need to review how we're handling Processor preprocesses and likely implement the same
  on-chain-preprocess-matches-presumed-preprocess check before publishing shares (though a delay of
  the re-attempt protocol's trigger length would also be sufficient).
*/
pub(crate) struct DkgConfirmer;
impl DkgConfirmer {
  fn preprocess_internal(
    spec: &TributarySpec,
    key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
    attempt: u32,
  ) -> (AlgorithmSignMachine<Ristretto, Schnorrkel>, [u8; 64]) {
    // TODO: Does Substrate already have a validator-uniqueness check?
    let validators = spec.validators().iter().map(|val| val.0).collect::<Vec<_>>();

    let context = musig_context(spec.set());
    let mut chacha = ChaCha20Rng::from_seed({
      let mut entropy_transcript = RecommendedTranscript::new(b"DkgConfirmer Entropy");
      entropy_transcript.append_message(b"spec", spec.serialize());
      entropy_transcript.append_message(b"key", Zeroizing::new(key.to_bytes()));
      entropy_transcript.append_message(b"attempt", attempt.to_le_bytes());
      Zeroizing::new(entropy_transcript).rng_seed(b"preprocess")
    });
    let (machine, preprocess) = AlgorithmMachine::new(
      Schnorrkel::new(b"substrate"),
      musig(&context, key, &validators)
        .expect("confirming the DKG for a set we aren't in/validator present multiple times")
        .into(),
    )
    .preprocess(&mut chacha);

    (machine, preprocess.serialize().try_into().unwrap())
  }
  // Get the preprocess for this confirmation.
  pub(crate) fn preprocess(
    spec: &TributarySpec,
    key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
    attempt: u32,
  ) -> [u8; 64] {
    Self::preprocess_internal(spec, key, attempt).1
  }

  fn share_internal(
    spec: &TributarySpec,
    key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
    attempt: u32,
    preprocesses: HashMap<Participant, Vec<u8>>,
    key_pair: &KeyPair,
  ) -> Result<(AlgorithmSignatureMachine<Ristretto, Schnorrkel>, [u8; 32]), Participant> {
    let machine = Self::preprocess_internal(spec, key, attempt).0;
    let preprocesses = preprocesses
      .into_iter()
      .map(|(p, preprocess)| {
        machine
          .read_preprocess(&mut preprocess.as_slice())
          .map(|preprocess| (p, preprocess))
          .map_err(|_| p)
      })
      .collect::<Result<HashMap<_, _>, _>>()?;
    let (machine, share) = machine
      .sign(preprocesses, &set_keys_message(&spec.set(), key_pair))
      .map_err(|e| match e {
        FrostError::InternalError(e) => unreachable!("FrostError::InternalError {e}"),
        FrostError::InvalidParticipant(_, _) |
        FrostError::InvalidSigningSet(_) |
        FrostError::InvalidParticipantQuantity(_, _) |
        FrostError::DuplicatedParticipant(_) |
        FrostError::MissingParticipant(_) => unreachable!("{e:?}"),
        FrostError::InvalidPreprocess(p) | FrostError::InvalidShare(p) => p,
      })?;

    Ok((machine, share.serialize().try_into().unwrap()))
  }
  // Get the share for this confirmation, if the preprocesses are valid.
  pub(crate) fn share(
    spec: &TributarySpec,
    key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
    attempt: u32,
    preprocesses: HashMap<Participant, Vec<u8>>,
    key_pair: &KeyPair,
  ) -> Result<[u8; 32], Participant> {
    Self::share_internal(spec, key, attempt, preprocesses, key_pair).map(|(_, share)| share)
  }

  pub(crate) fn complete(
    spec: &TributarySpec,
    key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
    attempt: u32,
    preprocesses: HashMap<Participant, Vec<u8>>,
    key_pair: &KeyPair,
    shares: HashMap<Participant, Vec<u8>>,
  ) -> Result<[u8; 64], Participant> {
    let machine = Self::share_internal(spec, key, attempt, preprocesses, key_pair)
      .expect("trying to complete a machine which failed to preprocess")
      .0;

    let shares = shares
      .into_iter()
      .map(|(p, share)| {
        machine.read_share(&mut share.as_slice()).map(|share| (p, share)).map_err(|_| p)
      })
      .collect::<Result<HashMap<_, _>, _>>()?;
    let signature = machine.complete(shares).map_err(|e| match e {
      FrostError::InternalError(e) => unreachable!("FrostError::InternalError {e}"),
      FrostError::InvalidParticipant(_, _) |
      FrostError::InvalidSigningSet(_) |
      FrostError::InvalidParticipantQuantity(_, _) |
      FrostError::DuplicatedParticipant(_) |
      FrostError::MissingParticipant(_) => unreachable!("{e:?}"),
      FrostError::InvalidPreprocess(p) | FrostError::InvalidShare(p) => p,
    })?;

    Ok(signature.to_bytes())
  }
}
