//! Unit tests of the engine at the chain: chain preconditions of requests, the state that the
//! engine derives from confirmed transactions, and the transactions that it wants to send.
//!
//! The tests pass facts about the chain to the engine directly. The transactions in the facts are
//! the transactions of the trade that the engines computed. The test IDs refer to section 9.4 of
//! `concept/CompletionPlan.md`.

use std::sync::Arc;

use super::tests::{ROLE_PAIRS, Trader, config, traders_after_deposit_signature};
use super::*;
use crate::transaction::REPLACEMENT_FEE_RATE_STEP;

/// The height of the block that contains the deposit transaction in these tests.
const DEPOSIT_HEIGHT: u32 = 1_000;
pub(super) const WARNING_LOCK_TIME: u32 = 20;
const CLAIM_LOCK_TIME: u32 = 10;
const RELEASE_SAFETY_MARGIN: u32 = 6;
pub(super) const FEE_RATE: FeeRate = FeeRate::from_sat_per_kwu(2_500);

/// A buyer and a seller who have signed the deposit transaction, for one of the four pairs of
/// roles.
pub(super) fn trade(roles: (Role, Role)) -> (Trader, Trader) {
    let (me, peer) = traders_after_deposit_signature(roles);
    if me.model.am_buyer() {
        (me, peer)
    } else {
        (peer, me)
    }
}

/// Both possible pairs of maker and taker, with the buyer first.
pub(super) const BUYER_SELLER_ROLES: [(Role, Role); 2] = [ROLE_PAIRS[0], ROLE_PAIRS[1]];

/// The facts when the deposit transaction has `deposit_confirmations` confirmations, and the
/// given transactions are confirmed with the given number of confirmations.
pub(super) fn facts(deposit_confirmations: u32, confirmed: &[(&Transaction, u32)]) -> ChainFacts {
    let mut spends = BTreeMap::new();
    for (tx, confirmations) in confirmed {
        for input in &tx.input {
            spends.insert(
                input.previous_output,
                Spend {
                    tx: Arc::new((*tx).clone()),
                    confirmations: *confirmations,
                },
            );
        }
    }
    ChainFacts {
        tip_height: DEPOSIT_HEIGHT + deposit_confirmations.saturating_sub(1),
        deposit_confirmations,
        spends,
    }
}

pub(super) fn observe(trader: &mut Trader, facts: &ChainFacts) -> Vec<TradeEvent> {
    trader
        .model
        .observe_chain(&mut *trader.wallet, facts.clone())
}

pub(super) fn observe_both(buyer: &mut Trader, seller: &mut Trader, facts: &ChainFacts) {
    observe(buyer, facts);
    observe(seller, facts);
}

pub(super) fn kinds(trader: &Trader) -> Vec<TxKind> {
    trader
        .model
        .transactions_to_send(FEE_RATE)
        .into_iter()
        .map(|to_send| to_send.kind)
        .collect()
}

pub(super) fn tx_to_send(trader: &Trader, kind: TxKind) -> Transaction {
    trader
        .model
        .transactions_to_send(FEE_RATE)
        .into_iter()
        .find(|to_send| to_send.kind == kind)
        .unwrap_or_else(|| panic!("the trader wants to send the {kind:?} transaction"))
        .tx
}

fn assert_chain_condition<T: std::fmt::Debug>(result: &Result<T>, part_of_reason: &str) {
    match result {
        Err(ProtocolErrorKind::ChainCondition(reason)) => {
            assert!(reason.contains(part_of_reason), "{reason}");
        }
        other => panic!("expected a refusal because of the chain, got {other:?}"),
    }
}

/// Messages E and F: the buyer starts the payment, the seller takes the signature and releases.
pub(super) fn start_payment_and_release(buyer: &mut Trader, seller: &mut Trader) -> Scalar {
    buyer.model.start_payment().expect("payment starts");
    let signature = *buyer
        .model
        .my_partial_signatures_on_peer_txs()
        .expect("signatures exist")
        .swap_tx_input_partial_signature
        .expect("message E contains the signature");
    seller
        .model
        .receive_swap_tx_signature(Ok(signature))
        .expect("valid signature");
    seller.model.release_payout().expect("released")
}

/// Messages E, F and G.
pub(super) fn close_cooperatively(buyer: &mut Trader, seller: &mut Trader) {
    let sellers_key_share = start_payment_and_release(buyer, seller);
    buyer
        .model
        .close_with_key_share(&mut *buyer.wallet, Ok(sellers_key_share), None)
        .expect("buyer closes");
    let buyers_key_share = *buyer
        .model
        .my_private_key_share_for_peer_output()
        .expect("message G");
    seller
        .model
        .close_with_key_share(&mut *seller.wallet, Ok(buyers_key_share), None)
        .expect("seller closes");
}

/// The publisher of a warning transaction and his peer.
fn pick<'a>(
    buyer_publishes: bool,
    buyer: &'a mut Trader,
    seller: &'a mut Trader,
) -> (&'a mut Trader, &'a mut Trader) {
    if buyer_publishes {
        (buyer, seller)
    } else {
        (seller, buyer)
    }
}

pub(super) fn closed(closure_type: ClosureType) -> TradeState {
    TradeState::TradeClosed(closure_type)
}

/// A14 (F9, D6): before the deposit transaction has 3 confirmations, the engine hands out no
/// signature for the swap transaction, no key share and no signature for a custom payout. At 3
/// confirmations it does. A request that was refused changes nothing.
#[test]
fn a14_requests_need_a_confirmed_deposit() {
    for roles in BUYER_SELLER_ROLES {
        let (mut buyer, mut seller) = trade(roles);
        for confirmations in 0..3 {
            observe_both(&mut buyer, &mut seller, &facts(confirmations, &[]));
            let reason = format!("{confirmations} of 3 confirmations");
            assert_chain_condition(&buyer.model.start_payment(), &reason);
            assert_chain_condition(
                &buyer.model.sign_custom_payout(
                    &mut *buyer.wallet,
                    Amount::from_sat(100_000),
                    FEE_RATE,
                ),
                &reason,
            );
            assert_chain_condition(
                &seller.model.sign_custom_payout(
                    &mut *seller.wallet,
                    Amount::from_sat(100_000),
                    FEE_RATE,
                ),
                &reason,
            );
            assert_eq!(buyer.model.state(), TradeState::Deposit);
            assert_eq!(seller.model.state(), TradeState::Deposit);
            let signatures = buyer.model.my_partial_signatures_on_peer_txs().unwrap();
            assert!(signatures.swap_tx_input_partial_signature.is_none());
        }

        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        buyer.model.start_payment().expect("confirmed deposit");
        let signature = *buyer
            .model
            .my_partial_signatures_on_peer_txs()
            .unwrap()
            .swap_tx_input_partial_signature
            .expect("message E");
        seller
            .model
            .receive_swap_tx_signature(Ok(signature))
            .expect("valid signature");

        // J1: a reorganisation lowers the depth of the deposit below 3 again. The seller does not
        // release, and the buyer does not hand out his key share.
        observe_both(&mut buyer, &mut seller, &facts(2, &[]));
        assert_chain_condition(&seller.model.release_payout(), "2 of 3 confirmations");
        assert_eq!(seller.model.state(), TradeState::BuyerReadyToRelease);

        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        let sellers_key_share = seller.model.release_payout().expect("confirmed deposit");
        observe(&mut buyer, &facts(2, &[]));
        assert_chain_condition(
            &buyer
                .model
                .close_with_key_share(&mut *buyer.wallet, Ok(sellers_key_share), None),
            "2 of 3 confirmations",
        );
        assert_eq!(buyer.model.state(), TradeState::BuyerReadyToRelease);
    }
}

/// F6, C3: the seller does not release when fewer blocks than the release safety margin remain
/// before the buyer's warning transaction becomes valid.
#[test]
fn f6_release_is_refused_inside_the_safety_margin() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    buyer.model.start_payment().expect("payment starts");
    let signature = *buyer
        .model
        .my_partial_signatures_on_peer_txs()
        .unwrap()
        .swap_tx_input_partial_signature
        .unwrap();
    seller
        .model
        .receive_swap_tx_signature(Ok(signature))
        .expect("valid signature");

    // 5 blocks remain: one block less than the margin.
    let too_late = WARNING_LOCK_TIME - RELEASE_SAFETY_MARGIN + 1;
    observe(&mut seller, &facts(too_late, &[]));
    assert_chain_condition(&seller.model.release_payout(), "5 blocks remain");
    assert_eq!(seller.model.state(), TradeState::BuyerReadyToRelease);
    assert!(seller.model.my_private_key_share_for_peer_output().is_err());

    // 6 blocks remain: the margin itself.
    observe(
        &mut seller,
        &facts(WARNING_LOCK_TIME - RELEASE_SAFETY_MARGIN, &[]),
    );
    // At the margin the seller's engine also closes by force on its own, so the release is
    // possible one block earlier at the latest.
    observe(
        &mut seller,
        &facts(WARNING_LOCK_TIME - RELEASE_SAFETY_MARGIN - 1, &[]),
    );
    seller.model.release_payout().expect("7 blocks remain");
    assert_eq!(seller.model.state(), TradeState::SellerReadyToRelease);
}

/// C1, C2, C4 (D7): the seller released and gets no answer. His engine closes by force on its
/// own when the remaining blocks fall to the release safety margin, and sends the swap
/// transaction. The buyer's engine reads the key share from the confirmed spend of the seller's
/// payout and closes. J4: when a reorganisation removes the swap transaction, the seller sends
/// it again, and the buyer keeps the key.
#[test]
fn c1_c4_forced_close_and_key_share_from_the_spend() {
    for roles in BUYER_SELLER_ROLES {
        let (mut buyer, mut seller) = trade(roles);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        start_payment_and_release(&mut buyer, &mut seller);
        assert_eq!(kinds(&seller), vec![]);

        // One block before the margin nothing happens.
        let at_margin = WARNING_LOCK_TIME - RELEASE_SAFETY_MARGIN;
        observe(&mut seller, &facts(at_margin - 1, &[]));
        assert_eq!(seller.model.state(), TradeState::SellerReadyToRelease);
        assert_eq!(kinds(&seller), vec![]);

        let events = observe(&mut seller, &facts(at_margin, &[]));
        assert_eq!(seller.model.state(), closed(ClosureType::Forced));
        assert_eq!(
            events,
            vec![TradeEvent::StateChanged {
                from: TradeState::SellerReadyToRelease,
                to: closed(ClosureType::Forced),
            }]
        );
        assert_eq!(kinds(&seller), vec![TxKind::Swap]);
        let swap_tx = tx_to_send(&seller, TxKind::Swap);
        assert!(!seller.model.is_final());

        // The buyer knows nothing of the swap transaction until it is in a block.
        observe(&mut buyer, &facts(at_margin, &[]));
        assert_eq!(buyer.model.state(), TradeState::BuyerReadyToRelease);
        let with_swap = facts(at_margin + 1, &[(&swap_tx, 1)]);
        let events = observe(&mut buyer, &with_swap);
        assert_eq!(buyer.model.state(), closed(ClosureType::Forced));
        assert_eq!(
            events,
            vec![TradeEvent::StateChanged {
                from: TradeState::BuyerReadyToRelease,
                to: closed(ClosureType::Forced),
            }]
        );
        // The buyer holds the complete key of his payout.
        buyer
            .model
            .keys
            .my_payout_ctx()
            .aggregated_key()
            .unwrap()
            .prv_key()
            .expect("the key share of the seller was read from the signature");
        observe(&mut seller, &with_swap);
        assert_eq!(
            kinds(&seller),
            vec![],
            "a confirmed transaction is not sent"
        );
        assert_eq!(kinds(&buyer), vec![]);

        // J4: the block with the swap transaction is replaced.
        observe_both(&mut buyer, &mut seller, &facts(at_margin + 2, &[]));
        assert_eq!(kinds(&seller), vec![TxKind::Swap]);
        assert_eq!(buyer.model.state(), closed(ClosureType::Forced));

        let final_facts = facts(at_margin + 5, &[(&swap_tx, 3)]);
        observe_both(&mut buyer, &mut seller, &final_facts);
        assert!(buyer.model.is_final() && seller.model.is_final());
    }
}

/// D23: with a forced close margin in the configuration, the seller's engine closes by force
/// when that number of blocks remains, which is earlier than at the release safety margin. The
/// rule for the release does not change.
#[test]
fn d23_forced_close_margin_of_the_configuration() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    let forced_close_margin = 12;
    seller.model.received.config = Some(
        config()
            .with_forced_close_margin(forced_close_margin)
            .expect("valid"),
    );
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    start_payment_and_release(&mut buyer, &mut seller);

    // 13 blocks remain: nothing happens.
    observe(&mut seller, &facts(WARNING_LOCK_TIME - 13, &[]));
    assert_eq!(seller.model.state(), TradeState::SellerReadyToRelease);
    assert_eq!(kinds(&seller), vec![]);
    // 12 blocks remain: the forced close margin.
    observe(&mut seller, &facts(WARNING_LOCK_TIME - 12, &[]));
    assert_eq!(seller.model.state(), closed(ClosureType::Forced));
    assert_eq!(kinds(&seller), vec![TxKind::Swap]);

    // A release is still possible while more blocks than the release safety margin remain. The
    // engine then closes by force at its next look at the chain.
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    seller.model.received.config = Some(
        config()
            .with_forced_close_margin(forced_close_margin)
            .expect("valid"),
    );
    let late = facts(WARNING_LOCK_TIME - RELEASE_SAFETY_MARGIN - 1, &[]);
    observe_both(&mut buyer, &mut seller, &late);
    start_payment_and_release(&mut buyer, &mut seller);
    assert_eq!(seller.model.state(), TradeState::SellerReadyToRelease);
    observe(&mut seller, &late);
    assert_eq!(seller.model.state(), closed(ClosureType::Forced));
}

/// C1 on request: the seller can ask for the forced close at any time after the release.
#[test]
fn c1_forced_close_on_request() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    start_payment_and_release(&mut buyer, &mut seller);
    seller
        .model
        .close_by_force()
        .expect("allowed after the release");
    assert_eq!(kinds(&seller), vec![TxKind::Swap]);
}

/// E1, E2, E14: a trader publishes his warning transaction on request, when its lock time has
/// passed. The peer's engine answers the confirmed warning with its redirect transaction, and
/// both trades close when the redirect is confirmed. J3: a reorganisation that removes the
/// warning takes the trade back to the state before.
#[test]
fn e1_e2_warning_and_redirect() {
    for (roles, buyer_warns) in [
        (BUYER_SELLER_ROLES[0], true),
        (BUYER_SELLER_ROLES[1], false),
    ] {
        let (mut buyer, mut seller) = trade(roles);
        let (warning_state, redirect_closure) = if buyer_warns {
            (TradeState::BuyersWarning, ClosureType::SellersRedirect)
        } else {
            (TradeState::SellersWarning, ClosureType::BuyersRedirect)
        };

        // One block before the lock time has passed, the request is refused.
        observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME - 1, &[]));
        {
            let (publisher, _) = pick(buyer_warns, &mut buyer, &mut seller);
            assert_chain_condition(&publisher.model.start_warning(), "valid in 1 blocks");
            assert_eq!(publisher.model.state(), TradeState::Deposit);
            assert_eq!(kinds(publisher), vec![]);
        }

        let unspent = facts(WARNING_LOCK_TIME, &[]);
        observe_both(&mut buyer, &mut seller, &unspent);
        let warning_tx = {
            let (publisher, _) = pick(buyer_warns, &mut buyer, &mut seller);
            let warning_tx = publisher
                .model
                .start_warning()
                .expect("lock time has passed");
            assert_eq!(publisher.model.state(), warning_state);
            assert_eq!(kinds(publisher), vec![TxKind::Warning]);
            // A repeated request changes nothing.
            assert_eq!(publisher.model.start_warning().unwrap(), warning_tx);
            warning_tx
        };
        {
            let (_, peer) = pick(buyer_warns, &mut buyer, &mut seller);
            assert_eq!(peer.model.state(), TradeState::Deposit);
            assert_eq!(kinds(peer), vec![]);
        }

        // The warning is confirmed. The peer sees it and wants to send his redirect.
        let warned = facts(WARNING_LOCK_TIME + 1, &[(&warning_tx, 1)]);
        observe_both(&mut buyer, &mut seller, &warned);
        assert_eq!(buyer.model.state(), warning_state);
        assert_eq!(seller.model.state(), warning_state);
        let redirect_tx = {
            let (publisher, peer) = pick(buyer_warns, &mut buyer, &mut seller);
            assert_eq!(
                kinds(publisher),
                vec![],
                "the claim lock time has not passed"
            );
            assert_eq!(kinds(peer), vec![TxKind::Redirect]);
            tx_to_send(peer, TxKind::Redirect)
        };

        // J3: the warning is removed by a reorganisation and confirmed again.
        observe_both(&mut buyer, &mut seller, &unspent);
        {
            let (publisher, peer) = pick(buyer_warns, &mut buyer, &mut seller);
            assert_eq!(peer.model.state(), TradeState::Deposit);
            assert_eq!(kinds(peer), vec![]);
            assert_eq!(publisher.model.state(), warning_state);
            assert_eq!(kinds(publisher), vec![TxKind::Warning]);
        }
        observe_both(&mut buyer, &mut seller, &warned);

        let redirected = facts(
            WARNING_LOCK_TIME + 2,
            &[(&warning_tx, 2), (&redirect_tx, 1)],
        );
        let events = observe(&mut buyer, &redirected);
        observe(&mut seller, &redirected);
        assert_eq!(buyer.model.state(), closed(redirect_closure));
        assert_eq!(seller.model.state(), closed(redirect_closure));
        assert_eq!(
            events,
            vec![TradeEvent::StateChanged {
                from: warning_state,
                to: closed(redirect_closure),
            }]
        );
        assert_eq!(kinds(&buyer), vec![]);
        assert_eq!(kinds(&seller), vec![]);
        assert!(!buyer.model.is_final());
        let final_facts = facts(
            WARNING_LOCK_TIME + 4,
            &[(&warning_tx, 4), (&redirect_tx, 3)],
        );
        observe_both(&mut buyer, &mut seller, &final_facts);
        assert!(buyer.model.is_final() && seller.model.is_final());
    }
}

/// E3, E4, E5 (D3): the peer does not react to the warning. When the claim lock time has
/// passed, the publisher's engine builds, signs and wants to send the claim. When it is
/// confirmed, the publisher's trade is closed, and the peer's engine reports the loss.
#[test]
fn e3_e5_claim_after_the_claim_lock_time() {
    for (roles, buyer_warns) in [
        (BUYER_SELLER_ROLES[0], true),
        (BUYER_SELLER_ROLES[1], false),
    ] {
        let (mut buyer, mut seller) = trade(roles);
        let claim_closure = if buyer_warns {
            ClosureType::BuyersClaim
        } else {
            ClosureType::SellersClaim
        };
        observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
        let (publisher, peer) = if buyer_warns {
            (&mut buyer, &mut seller)
        } else {
            (&mut seller, &mut buyer)
        };
        let warning_tx = publisher
            .model
            .start_warning()
            .expect("lock time has passed");

        let before = facts(
            WARNING_LOCK_TIME + CLAIM_LOCK_TIME - 1,
            &[(&warning_tx, CLAIM_LOCK_TIME - 1)],
        );
        observe(publisher, &before);
        assert_eq!(kinds(publisher), vec![], "one block too early");

        let after = facts(
            WARNING_LOCK_TIME + CLAIM_LOCK_TIME,
            &[(&warning_tx, CLAIM_LOCK_TIME)],
        );
        observe(publisher, &after);
        assert_eq!(kinds(publisher), vec![TxKind::Claim]);
        let claim_tx = tx_to_send(publisher, TxKind::Claim);
        assert_eq!(claim_tx.input[0].sequence.0, CLAIM_LOCK_TIME);
        // The engine names the same transaction every time.
        assert_eq!(tx_to_send(publisher, TxKind::Claim), claim_tx);

        let claimed = facts(
            WARNING_LOCK_TIME + CLAIM_LOCK_TIME + 1,
            &[(&warning_tx, CLAIM_LOCK_TIME + 1), (&claim_tx, 1)],
        );
        observe(publisher, &claimed);
        assert_eq!(publisher.model.state(), closed(claim_closure));
        assert_eq!(kinds(publisher), vec![]);

        let events = observe(peer, &claimed);
        assert_eq!(peer.model.state(), closed(claim_closure));
        assert!(
            matches!(&events[..], [TradeEvent::StateChanged { .. }, TradeEvent::Loss(loss)]
                if loss.contains("claimed all funds")),
            "{events:?}"
        );
    }
}

/// F1, F3, M1 (D8): after a cooperative close a trader publishes his warning transaction, for
/// example with a second copy of the trade data. The engine of the other trader answers with the
/// penalty transaction. The cheater's engine reports the loss.
#[test]
fn f1_penalty_after_a_cooperative_close() {
    for (roles, buyer_cheats) in [
        (BUYER_SELLER_ROLES[0], true),
        (BUYER_SELLER_ROLES[1], false),
    ] {
        let (mut buyer, mut seller) = trade(roles);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        close_cooperatively(&mut buyer, &mut seller);
        let cooperative = closed(ClosureType::Cooperative);
        assert_eq!(buyer.model.state(), cooperative);
        assert_eq!(seller.model.state(), cooperative);
        assert!(
            !buyer.model.is_final(),
            "watched while the payouts are unspent"
        );

        observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
        let (cheater, honest) = if buyer_cheats {
            (&mut buyer, &mut seller)
        } else {
            (&mut seller, &mut buyer)
        };
        // F3: the engine refuses to publish the warning of a closed trade.
        assert!(matches!(
            cheater.model.start_warning(),
            Err(ProtocolErrorKind::NotAllowedInState(_))
        ));
        assert_eq!(kinds(cheater), vec![]);
        let warning_tx = cheater.model.my_signed_warning_tx().unwrap().clone();

        let warned = facts(WARNING_LOCK_TIME + 1, &[(&warning_tx, 1)]);
        observe(honest, &warned);
        assert_eq!(honest.model.state(), cooperative);
        assert_eq!(kinds(honest), vec![TxKind::Penalty]);
        let penalty_tx = tx_to_send(honest, TxKind::Penalty);
        assert_eq!(
            penalty_tx.input[0].previous_output,
            OutPoint::new(warning_tx.compute_txid(), 0)
        );

        let penalised = facts(WARNING_LOCK_TIME + 2, &[(&warning_tx, 2), (&penalty_tx, 1)]);
        let penalty_closure = if buyer_cheats {
            ClosureType::SellersPenaltyTx
        } else {
            ClosureType::BuyersPenaltyTx
        };
        let events = observe(honest, &penalised);
        assert_eq!(honest.model.state(), closed(penalty_closure));
        assert_eq!(
            events,
            vec![TradeEvent::StateChanged {
                from: cooperative,
                to: closed(penalty_closure),
            }]
        );
        let events = observe(cheater, &penalised);
        assert_eq!(cheater.model.state(), closed(penalty_closure));
        assert!(
            matches!(&events[..], [TradeEvent::StateChanged { .. }, TradeEvent::Loss(loss)]
                if loss.contains("penalty transaction")),
            "{events:?}"
        );
    }
}

/// F2, M2, M3: after a cooperative close, a payment of a trader that spends his payout ends the
/// protection for both traders. Nothing is sent, and the trades are final.
#[test]
fn m2_spent_payout_ends_the_watching_of_a_closed_trade() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    close_cooperatively(&mut buyer, &mut seller);
    let buyer_payout = buyer
        .model
        .deposit_tx
        .builder
        .buyer_payout()
        .unwrap()
        .clone();
    let payment = Transaction {
        version: bdk_wallet::bitcoin::transaction::Version::TWO,
        lock_time: bdk_wallet::bitcoin::absolute::LockTime::ZERO,
        input: vec![bdk_wallet::bitcoin::TxIn {
            previous_output: buyer_payout.outpoint,
            ..Default::default()
        }],
        output: Vec::new(),
    };
    let spent = facts(8, &[(&payment, 3)]);
    for trader in [&mut buyer, &mut seller] {
        let events = observe(trader, &spent);
        assert_eq!(events, vec![]);
        assert_eq!(trader.model.state(), closed(ClosureType::Cooperative));
        assert_eq!(kinds(trader), vec![]);
        assert!(trader.model.is_final());
    }
}

/// E6: both traders decide to publish their warning transaction. Only one confirms. The trader
/// whose warning did not confirm treats the confirmed one as the warning of the peer.
#[test]
fn e6_two_competing_warnings() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
    buyer.model.start_warning().expect("buyer decides");
    let sellers_warning = seller.model.start_warning().expect("seller decides");
    assert_eq!(kinds(&buyer), vec![TxKind::Warning]);
    assert_eq!(kinds(&seller), vec![TxKind::Warning]);

    let warned = facts(WARNING_LOCK_TIME + 1, &[(&sellers_warning, 1)]);
    observe_both(&mut buyer, &mut seller, &warned);
    assert_eq!(buyer.model.state(), TradeState::SellersWarning);
    assert_eq!(seller.model.state(), TradeState::SellersWarning);
    assert_eq!(kinds(&buyer), vec![TxKind::Redirect]);
    assert_eq!(kinds(&seller), vec![]);
}

/// E11, C4: the seller released, and the buyer publishes his warning transaction instead of
/// answering. The seller cannot use the penalty, because he does not hold the key of his payout.
/// His engine redirects.
#[test]
fn e11_buyers_warning_after_the_release() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    start_payment_and_release(&mut buyer, &mut seller);
    // The seller's engine closes by force before the warning is valid. The swap transaction does
    // not confirm in this test.
    observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
    assert_eq!(seller.model.state(), closed(ClosureType::Forced));
    assert_eq!(kinds(&seller), vec![TxKind::Swap]);
    let warning_tx = buyer.model.start_warning().expect("buyer decides");

    let warned = facts(WARNING_LOCK_TIME + 1, &[(&warning_tx, 1)]);
    observe(&mut seller, &warned);
    assert_eq!(seller.model.state(), TradeState::BuyersWarning);
    // The swap transaction is void now, and it is not sent any more.
    assert_eq!(kinds(&seller), vec![TxKind::Redirect]);
}

/// D1, D5: the custom payout transaction is sent by the trader who finalised it, and both
/// trades close when it is confirmed, also the trade of the trader who only signed.
#[test]
fn d1_custom_payout_on_chain() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    for trader in [&mut buyer, &mut seller] {
        trader
            .model
            .sign_custom_payout(&mut *trader.wallet, Amount::from_sat(100_000), FEE_RATE)
            .expect("signed");
        assert_eq!(kinds(trader), vec![]);
    }
    let custom_payout_txid = buyer
        .model
        .custom_payout_psbt()
        .unwrap()
        .unsigned_tx
        .compute_txid();
    // The transaction as it would be in a block. The signatures of the mock wallets are not
    // valid, so the test uses the unsigned transaction with the same ID.
    let custom_payout_tx = buyer
        .model
        .custom_payout_psbt()
        .unwrap()
        .unsigned_tx
        .clone();
    assert_eq!(custom_payout_tx.compute_txid(), custom_payout_txid);

    let paid = facts(5, &[(&custom_payout_tx, 1)]);
    for trader in [&mut buyer, &mut seller] {
        let events = observe(trader, &paid);
        assert_eq!(trader.model.state(), closed(ClosureType::Custom));
        assert_eq!(
            events,
            vec![TradeEvent::StateChanged {
                from: TradeState::CustomPayoutSigned,
                to: closed(ClosureType::Custom),
            }]
        );
        assert_eq!(kinds(trader), vec![]);
    }
}

/// The facts when the deposit transaction is unconfirmed, `blocks_passed` blocks after the first
/// observation of these tests.
pub(super) fn unconfirmed(blocks_passed: u32) -> ChainFacts {
    ChainFacts {
        tip_height: DEPOSIT_HEIGHT + blocks_passed,
        ..ChainFacts::default()
    }
}

/// A buyer and a seller who have signed the deposit transaction. The buyer is the taker and
/// holds the deposit transaction with all signatures.
fn trade_with_signed_deposit() -> (Trader, Trader) {
    let (mut buyer, seller) = trade(BUYER_SELLER_ROLES[0]);
    let sellers_deposit_psbt = seller.model.deposit_psbt().unwrap().clone();
    buyer
        .model
        .receive_deposit_psbt(&mut *buyer.wallet, Ok(sellers_deposit_psbt))
        .expect("signed by both");
    (buyer, seller)
}

fn fee(tx: &Transaction, inputs: &Psbt) -> Amount {
    let paid: Amount = inputs
        .unsigned_tx
        .input
        .iter()
        .zip(&inputs.inputs)
        .filter(|(tx_in, _)| {
            tx.input
                .iter()
                .any(|spent| spent.previous_output == tx_in.previous_output)
        })
        .map(|(_, input)| input.witness_utxo.as_ref().unwrap().value)
        .sum();
    paid - tx.output.iter().map(|output| output.value).sum()
}

/// A1 (N2): the trader who holds all signatures wants to send the deposit transaction until it
/// is confirmed, and until the deposit timeout has passed.
#[test]
fn a1_deposit_is_sent_until_it_is_confirmed_or_timed_out() {
    let (mut buyer, mut seller) = trade_with_signed_deposit();
    observe_both(&mut buyer, &mut seller, &unconfirmed(0));
    assert_eq!(kinds(&buyer), vec![TxKind::Deposit]);
    assert_eq!(
        kinds(&seller),
        vec![],
        "the maker does not hold all signatures"
    );

    let timeout = u32::from(config().deposit_timeout());
    observe(&mut buyer, &unconfirmed(timeout));
    assert_eq!(kinds(&buyer), vec![TxKind::Deposit]);
    observe(&mut buyer, &unconfirmed(timeout + 1));
    assert_eq!(
        kinds(&buyer),
        vec![TxKind::DepositAbort],
        "the deposit timeout has passed"
    );

    observe(&mut buyer, &facts(1, &[]));
    assert_eq!(kinds(&buyer), vec![], "a confirmed deposit is not sent");
    assert_eq!(buyer.model.state(), TradeState::Deposit);
}

/// A3 (D13): when the deposit timeout has passed without a confirmation of the deposit
/// transaction, each trader wants to send his deposit abort transaction. It spends the coins
/// that he put into the deposit, pays to his own address, and pays more than the whole deposit
/// transaction, so that it replaces the deposit transaction in a node that holds it.
#[test]
fn a3_deposit_abort_after_the_timeout() {
    let (mut buyer, mut seller) = trade_with_signed_deposit();
    let timeout = u32::from(config().deposit_timeout());
    observe_both(&mut buyer, &mut seller, &unconfirmed(0));
    observe_both(&mut buyer, &mut seller, &unconfirmed(timeout + 1));

    let deposit_psbt = buyer.model.deposit_psbt().unwrap().clone();
    let deposit_tx = buyer.model.signed_deposit_tx().unwrap();
    let deposit_fee = fee(&deposit_tx, &deposit_psbt);
    for trader in [&mut buyer, &mut seller] {
        assert_eq!(kinds(trader), vec![TxKind::DepositAbort]);
        let abort_tx = tx_to_send(trader, TxKind::DepositAbort);
        let spent = abort_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>();
        assert_eq!(spent, trader.model.reserved_coins());
        assert!(abort_tx.input.iter().all(|input| input.sequence.is_rbf()));
        let own_address = trader.model.my_addresses().unwrap().claim_tx_payout;
        assert_eq!(abort_tx.output.len(), 1);
        assert_eq!(
            abort_tx.output[0].script_pubkey,
            own_address.script_pubkey()
        );
        let abort_fee = fee(&abort_tx, &deposit_psbt);
        let step = REPLACEMENT_FEE_RATE_STEP
            .checked_mul_by_weight(abort_tx.weight())
            .unwrap();
        assert!(
            abort_fee >= deposit_fee + step,
            "{abort_fee} replaces a deposit transaction with the fee {deposit_fee}"
        );
        assert!(abort_tx.weight() < deposit_tx.weight());
        assert!(
            !trader.model.reserved_coins().is_empty(),
            "the coins stay reserved until one of the two transactions is confirmed"
        );
        assert_eq!(trader.model.state(), TradeState::Deposit);

        // The next look at the chain names the same transaction.
        observe(trader, &unconfirmed(timeout + 2));
        assert_eq!(tx_to_send(trader, TxKind::DepositAbort), abort_tx);
    }

    // The abort of the seller confirms. Both trades are closed and the coins are free.
    let sellers_abort_tx = tx_to_send(&seller, TxKind::DepositAbort);
    let aborted = ChainFacts {
        tip_height: DEPOSIT_HEIGHT + timeout + 3,
        ..facts(0, &[(&sellers_abort_tx, 1)])
    };
    for trader in [&mut buyer, &mut seller] {
        observe(trader, &aborted);
        assert_eq!(trader.model.state(), closed(ClosureType::DepositAborted));
        assert_eq!(trader.model.reserved_coins(), &[]);
        assert_eq!(kinds(trader), vec![]);
    }
}

/// A3, A5: the engine follows the transaction that confirms. A deposit transaction that
/// confirms after the timeout continues the trade, and the abort is no longer sent. If a
/// reorganisation removes the deposit transaction again, the timeout starts again.
#[test]
fn a5_deposit_confirms_after_the_timeout() {
    let (mut buyer, mut seller) = trade_with_signed_deposit();
    let timeout = u32::from(config().deposit_timeout());
    observe_both(&mut buyer, &mut seller, &unconfirmed(0));
    observe_both(&mut buyer, &mut seller, &unconfirmed(timeout + 1));
    assert_eq!(kinds(&buyer), vec![TxKind::DepositAbort]);
    assert_eq!(kinds(&seller), vec![TxKind::DepositAbort]);

    let confirmed = ChainFacts {
        tip_height: DEPOSIT_HEIGHT + timeout + 2,
        deposit_confirmations: 1,
        ..ChainFacts::default()
    };
    observe_both(&mut buyer, &mut seller, &confirmed);
    for trader in [&buyer, &seller] {
        assert_eq!(kinds(trader), vec![]);
        assert_eq!(trader.model.state(), TradeState::Deposit);
        assert!(!trader.model.reserved_coins().is_empty());
    }

    // A reorganisation removes the deposit transaction. The taker sends it again, and no
    // trader aborts before the timeout has passed once more.
    let removed_at = timeout + 3;
    observe_both(&mut buyer, &mut seller, &unconfirmed(removed_at));
    assert_eq!(kinds(&buyer), vec![TxKind::Deposit]);
    assert_eq!(kinds(&seller), vec![]);
    observe_both(&mut buyer, &mut seller, &unconfirmed(removed_at + timeout));
    assert_eq!(kinds(&buyer), vec![TxKind::Deposit]);
    observe_both(
        &mut buyer,
        &mut seller,
        &unconfirmed(removed_at + timeout + 1),
    );
    assert_eq!(kinds(&buyer), vec![TxKind::DepositAbort]);
    assert_eq!(kinds(&seller), vec![TxKind::DepositAbort]);
}

/// J1, D13: the engine does not abort the deposit on its own after the buyer has started the
/// payment. If a reorganisation removes the deposit transaction then, the trader who holds it
/// sends it again, also after the deposit timeout.
#[test]
fn j1_no_deposit_abort_after_the_payment_started() {
    let (mut buyer, mut seller) = trade_with_signed_deposit();
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    let signature = {
        buyer.model.start_payment().expect("payment starts");
        *buyer
            .model
            .my_partial_signatures_on_peer_txs()
            .unwrap()
            .swap_tx_input_partial_signature
            .expect("message E")
    };
    seller
        .model
        .receive_swap_tx_signature(Ok(signature))
        .expect("valid signature");

    let timeout = u32::from(config().deposit_timeout());
    observe_both(&mut buyer, &mut seller, &unconfirmed(10));
    observe_both(&mut buyer, &mut seller, &unconfirmed(10 + timeout + 1));
    assert_eq!(kinds(&buyer), vec![TxKind::Deposit]);
    assert_eq!(kinds(&seller), vec![]);
    assert_eq!(buyer.model.state(), TradeState::BuyerReadyToRelease);
    assert_eq!(seller.model.state(), TradeState::BuyerReadyToRelease);
}

/// A3, A13, J2: a confirmed transaction that is not the deposit transaction spends an input of
/// the deposit. The deposit can never confirm. The trade is closed as `DepositAborted`, and the
/// coins that the trader reserved are free again.
#[test]
fn a13_conflicting_spend_of_a_deposit_input_aborts_the_deposit() {
    for roles in BUYER_SELLER_ROLES {
        let (mut buyer, mut seller) = trade(roles);
        observe_both(&mut buyer, &mut seller, &ChainFacts::default());
        // The seller spends one of his deposit inputs in another transaction.
        let sellers_coin = seller.model.reserved_coins()[0];
        let conflict = Transaction {
            version: bdk_wallet::bitcoin::transaction::Version::TWO,
            lock_time: bdk_wallet::bitcoin::absolute::LockTime::ZERO,
            input: vec![bdk_wallet::bitcoin::TxIn {
                previous_output: sellers_coin,
                ..Default::default()
            }],
            output: Vec::new(),
        };
        let conflicting = facts(0, &[(&conflict, 1)]);
        for trader in [&mut buyer, &mut seller] {
            assert!(!trader.model.reserved_coins().is_empty());
            let events = observe(trader, &conflicting);
            assert_eq!(trader.model.state(), closed(ClosureType::DepositAborted));
            assert_eq!(
                events,
                vec![TradeEvent::StateChanged {
                    from: TradeState::Deposit,
                    to: closed(ClosureType::DepositAborted),
                }]
            );
            assert_eq!(trader.model.reserved_coins(), &[]);
            assert_eq!(trader.wallet.list_locked_utxos(), vec![]);
            assert_eq!(kinds(trader), vec![]);
            assert!(trader.model.start_payment().is_err());
            assert!(!trader.model.is_final());
        }
        let final_facts = facts(0, &[(&conflict, 3)]);
        observe_both(&mut buyer, &mut seller, &final_facts);
        assert!(buyer.model.is_final() && seller.model.is_final());
    }
}

/// O1 of the specification: a trade that was terminated after the deposit was signed keeps the
/// actions that the trader can perform alone, here the warning and the reaction to the warning
/// of the peer.
#[test]
fn terminated_trade_keeps_the_actions_of_the_trader_alone() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    buyer.model.terminate("test".to_owned());
    seller.model.terminate("test".to_owned());
    assert!(matches!(
        buyer.model.start_payment(),
        Err(ProtocolErrorKind::TradeTerminated(_))
    ));

    observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
    let warning_tx = buyer.model.start_warning().expect("allowed alone");
    assert_eq!(kinds(&buyer), vec![TxKind::Warning]);
    let warned = facts(WARNING_LOCK_TIME + 1, &[(&warning_tx, 1)]);
    observe(&mut seller, &warned);
    assert_eq!(kinds(&seller), vec![TxKind::Redirect]);
}
