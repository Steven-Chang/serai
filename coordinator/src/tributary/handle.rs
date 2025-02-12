use core::{ops::Deref, future::Future};

use zeroize::Zeroizing;

use ciphersuite::{group::GroupEncoding, Ciphersuite, Ristretto};
use frost::dkg::Participant;

use serai_client::{
  Signature,
  validator_sets::primitives::{ValidatorSet, KeyPair},
  subxt::utils::Encoded,
  SeraiValidatorSets,
};

use tributary::Signed;

use processor_messages::{
  key_gen::{self, KeyGenId},
  coordinator,
  sign::{self, SignId},
};

use serai_db::Db;

use crate::{
  processors::Processors,
  tributary::{
    Transaction, TributarySpec, Topic, DataSpecification, TributaryDb, DataSet, Accumulation,
    TributaryState,
    nonce_decider::NonceDecider,
    dkg_confirmer::DkgConfirmer,
    scanner::{RecognizedIdType, RIDTrait},
  },
};

const DKG_COMMITMENTS: &str = "commitments";
const DKG_SHARES: &str = "shares";
const DKG_CONFIRMATION_NONCES: &str = "confirmation_nonces";
const DKG_CONFIRMATION_SHARES: &str = "confirmation_shares";

// These s/b prefixes between Batch and Sign should be unnecessary, as Batch/Share entries
// themselves should already be domain separated
const BATCH_PREPROCESS: &str = "b_preprocess";
const BATCH_SHARE: &str = "b_share";

const SIGN_PREPROCESS: &str = "s_preprocess";
const SIGN_SHARE: &str = "s_share";

pub fn dkg_confirmation_nonces(
  key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  spec: &TributarySpec,
  attempt: u32,
) -> [u8; 64] {
  DkgConfirmer::preprocess(spec, key, attempt)
}

#[allow(clippy::needless_pass_by_ref_mut)]
pub fn generated_key_pair<D: Db>(
  txn: &mut D::Transaction<'_>,
  key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  spec: &TributarySpec,
  key_pair: &KeyPair,
  attempt: u32,
) -> Result<[u8; 32], Participant> {
  TributaryDb::<D>::save_currently_completing_key_pair(txn, spec.genesis(), key_pair);
  let preprocesses = TributaryDb::<D>::confirmation_nonces(txn, spec.genesis(), attempt).unwrap();
  DkgConfirmer::share(spec, key, attempt, preprocesses, key_pair)
}

pub(crate) fn fatal_slash<D: Db>(
  txn: &mut D::Transaction<'_>,
  genesis: [u8; 32],
  account: [u8; 32],
  reason: &str,
) {
  log::warn!("fatally slashing {}. reason: {}", hex::encode(account), reason);
  TributaryDb::<D>::set_fatally_slashed(txn, genesis, account);
  // TODO: disconnect the node from network/ban from further participation in all Tributaries
}

pub(crate) async fn handle_application_tx<
  D: Db,
  Pro: Processors,
  FPst: Future<Output = ()>,
  PST: Clone + Fn(ValidatorSet, Encoded) -> FPst,
  FRid: Future<Output = ()>,
  RID: RIDTrait<FRid>,
>(
  tx: Transaction,
  spec: &TributarySpec,
  processors: &Pro,
  publish_serai_tx: PST,
  key: &Zeroizing<<Ristretto as Ciphersuite>::F>,
  recognized_id: RID,
  txn: &mut <D as Db>::Transaction<'_>,
) {
  let genesis = spec.genesis();

  let handle = |txn: &mut <D as Db>::Transaction<'_>,
                data_spec: &DataSpecification,
                bytes: Vec<u8>,
                signed: &Signed| {
    let Some(curr_attempt) = TributaryDb::<D>::attempt(txn, genesis, data_spec.topic) else {
      // Premature publication of a valid ID/publication of an invalid ID
      fatal_slash::<D>(
        txn,
        genesis,
        signed.signer.to_bytes(),
        "published data for ID without an attempt",
      );
      return Accumulation::NotReady;
    };

    // If they've already published a TX for this attempt, slash
    if TributaryDb::<D>::data(txn, genesis, data_spec, signed.signer).is_some() {
      fatal_slash::<D>(txn, genesis, signed.signer.to_bytes(), "published data multiple times");
      return Accumulation::NotReady;
    }

    // If the attempt is lesser than the blockchain's, slash
    if data_spec.attempt < curr_attempt {
      // TODO: Slash for being late
      return Accumulation::NotReady;
    }
    // If the attempt is greater, this is a premature publication, full slash
    if data_spec.attempt > curr_attempt {
      fatal_slash::<D>(
        txn,
        genesis,
        signed.signer.to_bytes(),
        "published data with an attempt which hasn't started",
      );
      return Accumulation::NotReady;
    }

    // TODO: We can also full slash if shares before all commitments, or share before the
    // necessary preprocesses

    // TODO: If this is shares, we need to check they are part of the selected signing set

    // Accumulate this data
    TributaryState::<D>::accumulate(txn, key, spec, data_spec, signed.signer, &bytes)
  };

  match tx {
    Transaction::DkgCommitments(attempt, bytes, signed) => {
      match handle(
        txn,
        &DataSpecification { topic: Topic::Dkg, label: DKG_COMMITMENTS, attempt },
        bytes,
        &signed,
      ) {
        Accumulation::Ready(DataSet::Participating(commitments)) => {
          log::info!("got all DkgCommitments for {}", hex::encode(genesis));
          processors
            .send(
              spec.set().network,
              key_gen::CoordinatorMessage::Commitments {
                id: KeyGenId { set: spec.set(), attempt },
                commitments,
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {
          panic!("wasn't a participant in DKG commitments")
        }
        Accumulation::NotReady => {}
      }
    }

    Transaction::DkgShares { attempt, mut shares, confirmation_nonces, signed } => {
      if shares.len() != (usize::from(spec.n()) - 1) {
        fatal_slash::<D>(txn, genesis, signed.signer.to_bytes(), "invalid amount of DKG shares");
        return;
      }

      let sender_i = spec
        .i(signed.signer)
        .expect("transaction added to tributary by signer who isn't a participant");

      // Only save our share's bytes
      let our_i = spec
        .i(Ristretto::generator() * key.deref())
        .expect("in a tributary we're not a validator for");

      let bytes = if sender_i == our_i {
        vec![]
      } else {
        // 1-indexed to 0-indexed, handling the omission of the sender's own data
        let relative_i = usize::from(u16::from(our_i) - 1) -
          (if u16::from(our_i) > u16::from(sender_i) { 1 } else { 0 });
        // Safe since we length-checked shares
        shares.swap_remove(relative_i)
      };
      drop(shares);

      let confirmation_nonces = handle(
        txn,
        &DataSpecification { topic: Topic::Dkg, label: DKG_CONFIRMATION_NONCES, attempt },
        confirmation_nonces.to_vec(),
        &signed,
      );
      match handle(
        txn,
        &DataSpecification { topic: Topic::Dkg, label: DKG_SHARES, attempt },
        bytes,
        &signed,
      ) {
        Accumulation::Ready(DataSet::Participating(shares)) => {
          log::info!("got all DkgShares for {}", hex::encode(genesis));

          let Accumulation::Ready(DataSet::Participating(confirmation_nonces)) =
            confirmation_nonces
          else {
            panic!("got all DKG shares yet confirmation nonces aren't Ready(Participating(_))");
          };
          TributaryDb::<D>::save_confirmation_nonces(txn, genesis, attempt, confirmation_nonces);

          processors
            .send(
              spec.set().network,
              key_gen::CoordinatorMessage::Shares {
                id: KeyGenId { set: spec.set(), attempt },
                shares,
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {
          panic!("wasn't a participant in DKG shares")
        }
        Accumulation::NotReady => assert!(matches!(confirmation_nonces, Accumulation::NotReady)),
      }
    }

    Transaction::DkgConfirmed(attempt, shares, signed) => {
      match handle(
        txn,
        &DataSpecification { topic: Topic::Dkg, label: DKG_CONFIRMATION_SHARES, attempt },
        shares.to_vec(),
        &signed,
      ) {
        Accumulation::Ready(DataSet::Participating(shares)) => {
          log::info!("got all DkgConfirmed for {}", hex::encode(genesis));

          let preprocesses = TributaryDb::<D>::confirmation_nonces(txn, genesis, attempt).unwrap();
          // TODO: This can technically happen under very very very specific timing as the txn put
          // happens before DkgConfirmed, yet the txn commit isn't guaranteed to
          let key_pair = TributaryDb::<D>::currently_completing_key_pair(txn, genesis)
            .unwrap_or_else(|| {
              panic!(
                "in DkgConfirmed handling, which happens after everyone {}",
                "(including us) fires DkgConfirmed, yet no confirming key pair"
              )
            });
          let Ok(sig) = DkgConfirmer::complete(spec, key, attempt, preprocesses, &key_pair, shares)
          else {
            // TODO: Full slash
            todo!();
          };

          publish_serai_tx(
            spec.set(),
            SeraiValidatorSets::set_keys(spec.set().network, key_pair, Signature(sig)),
          )
          .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {
          panic!("wasn't a participant in DKG confirmination shares")
        }
        Accumulation::NotReady => {}
      }
    }

    Transaction::Batch(_, batch) => {
      // Because this Batch has achieved synchrony, its batch ID should be authorized
      TributaryDb::<D>::recognize_topic(txn, genesis, Topic::Batch(batch));
      let nonce = NonceDecider::<D>::handle_batch(txn, genesis, batch);
      recognized_id(spec.set(), genesis, RecognizedIdType::Batch, batch, nonce).await;
    }

    Transaction::SubstrateBlock(block) => {
      let plan_ids = TributaryDb::<D>::plan_ids(txn, genesis, block).expect(
        "synced a tributary block finalizing a substrate block in a provided transaction \
          despite us not providing that transaction",
      );

      let nonces = NonceDecider::<D>::handle_substrate_block(txn, genesis, &plan_ids);
      for (nonce, id) in nonces.into_iter().zip(plan_ids.into_iter()) {
        TributaryDb::<D>::recognize_topic(txn, genesis, Topic::Sign(id));
        recognized_id(spec.set(), genesis, RecognizedIdType::Plan, id, nonce).await;
      }
    }

    Transaction::BatchPreprocess(data) => {
      match handle(
        txn,
        &DataSpecification {
          topic: Topic::Batch(data.plan),
          label: BATCH_PREPROCESS,
          attempt: data.attempt,
        },
        data.data,
        &data.signed,
      ) {
        Accumulation::Ready(DataSet::Participating(preprocesses)) => {
          NonceDecider::<D>::selected_for_signing_batch(txn, genesis, data.plan);
          let key = TributaryDb::<D>::key_pair(txn, spec.set()).unwrap().0 .0.to_vec();
          processors
            .send(
              spec.set().network,
              coordinator::CoordinatorMessage::BatchPreprocesses {
                id: SignId { key, id: data.plan, attempt: data.attempt },
                preprocesses,
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {}
        Accumulation::NotReady => {}
      }
    }
    Transaction::BatchShare(data) => {
      match handle(
        txn,
        &DataSpecification {
          topic: Topic::Batch(data.plan),
          label: BATCH_SHARE,
          attempt: data.attempt,
        },
        data.data,
        &data.signed,
      ) {
        Accumulation::Ready(DataSet::Participating(shares)) => {
          let key = TributaryDb::<D>::key_pair(txn, spec.set()).unwrap().0 .0.to_vec();
          processors
            .send(
              spec.set().network,
              coordinator::CoordinatorMessage::BatchShares {
                id: SignId { key, id: data.plan, attempt: data.attempt },
                shares: shares
                  .into_iter()
                  .map(|(validator, share)| (validator, share.try_into().unwrap()))
                  .collect(),
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {}
        Accumulation::NotReady => {}
      }
    }

    Transaction::SignPreprocess(data) => {
      let key_pair = TributaryDb::<D>::key_pair(txn, spec.set());
      match handle(
        txn,
        &DataSpecification {
          topic: Topic::Sign(data.plan),
          label: SIGN_PREPROCESS,
          attempt: data.attempt,
        },
        data.data,
        &data.signed,
      ) {
        Accumulation::Ready(DataSet::Participating(preprocesses)) => {
          NonceDecider::<D>::selected_for_signing_plan(txn, genesis, data.plan);
          processors
            .send(
              spec.set().network,
              sign::CoordinatorMessage::Preprocesses {
                id: SignId {
                  key: key_pair
                    .expect("completed SignPreprocess despite not setting the key pair")
                    .1
                    .into(),
                  id: data.plan,
                  attempt: data.attempt,
                },
                preprocesses,
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {}
        Accumulation::NotReady => {}
      }
    }
    Transaction::SignShare(data) => {
      let key_pair = TributaryDb::<D>::key_pair(txn, spec.set());
      match handle(
        txn,
        &DataSpecification {
          topic: Topic::Sign(data.plan),
          label: SIGN_SHARE,
          attempt: data.attempt,
        },
        data.data,
        &data.signed,
      ) {
        Accumulation::Ready(DataSet::Participating(shares)) => {
          processors
            .send(
              spec.set().network,
              sign::CoordinatorMessage::Shares {
                id: SignId {
                  key: key_pair
                    .expect("completed SignShares despite not setting the key pair")
                    .1
                    .into(),
                  id: data.plan,
                  attempt: data.attempt,
                },
                shares,
              },
            )
            .await;
        }
        Accumulation::Ready(DataSet::NotParticipating) => {}
        Accumulation::NotReady => {}
      }
    }
    Transaction::SignCompleted { plan, tx_hash, .. } => {
      log::info!(
        "on-chain SignCompleted claims {} completes {}",
        hex::encode(&tx_hash),
        hex::encode(plan)
      );
      // TODO: Confirm this is a valid plan ID
      // TODO: Confirm this signer hasn't prior published a completion
      let Some(key_pair) = TributaryDb::<D>::key_pair(txn, spec.set()) else { todo!() };
      processors
        .send(
          spec.set().network,
          sign::CoordinatorMessage::Completed { key: key_pair.1.to_vec(), id: plan, tx: tx_hash },
        )
        .await;
    }
  }
}
