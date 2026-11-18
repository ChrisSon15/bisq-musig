//! Unit tests of the stored record of a trade: what it contains, and that a trade that is built
//! again from its record continues to the same result (group H of section 9.4 of
//! `concept/CompletionPlan.md`).

use bdk_wallet::bitcoin::hex::DisplayHex as _;

use super::chain_tests::{
    BUYER_SELLER_ROLES, FEE_RATE, WARNING_LOCK_TIME, close_cooperatively, closed, facts, kinds,
    observe, observe_both, start_payment_and_release, trade, tx_to_send, unconfirmed,
};
use super::tests::{Trader, config, new_trader_for_trade};
use super::*;
use crate::mocks::{mock_buyer_trade_wallet, mock_seller_trade_wallet};

/// What a restart does to a trader: the trade is built again from its record, with a wallet that
/// holds the same seed. Nothing else of the old trade is used.
fn restart(trader: &Trader) -> Trader {
    let record = trader.model.to_record().expect("record");
    let wallet: Box<dyn ProtocolWalletApi> = if trader.model.am_buyer() {
        Box::new(mock_buyer_trade_wallet())
    } else {
        Box::new(mock_seller_trade_wallet())
    };
    let model = TradeModel::from_record(&record, &*wallet).expect("the record can be loaded");
    // The record of the loaded trade is the record that was stored.
    assert_eq!(
        String::from_utf8(model.to_record().expect("record").to_vec()).unwrap(),
        String::from_utf8(record.to_vec()).unwrap()
    );
    assert_eq!(model.state(), trader.model.state);
    Trader { model, wallet }
}

fn restart_both(buyer: &mut Trader, seller: &mut Trader) {
    *buyer = restart(buyer);
    *seller = restart(seller);
}

fn record_text(trader: &Trader) -> String {
    String::from_utf8(trader.model.to_record().expect("record").to_vec()).expect("JSON text")
}

/// H2: both traders restart after every step of a cooperative close. The trade ends as without
/// a restart, and the penalty transactions exist.
#[test]
fn h2_restart_in_every_state_of_a_cooperative_close() {
    for roles in BUYER_SELLER_ROLES {
        let (mut buyer, mut seller) = trade(roles);
        restart_both(&mut buyer, &mut seller);
        assert_eq!(buyer.model.state(), TradeState::Deposit);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));

        buyer.model.start_payment().expect("payment starts");
        restart_both(&mut buyer, &mut seller);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        // H3: the request is repeated after the restart and gives the same signature.
        buyer.model.start_payment().expect("repeated request");
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
        restart_both(&mut buyer, &mut seller);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        assert_eq!(seller.model.state(), TradeState::BuyerReadyToRelease);
        seller
            .model
            .receive_swap_tx_signature(Ok(signature))
            .expect("repeated request");

        let sellers_key_share = seller.model.release_payout().expect("released");
        restart_both(&mut buyer, &mut seller);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        assert_eq!(seller.model.state(), TradeState::SellerReadyToRelease);
        assert_eq!(seller.model.release_payout().unwrap(), sellers_key_share);

        buyer
            .model
            .close_with_key_share(&mut *buyer.wallet, Ok(sellers_key_share), None)
            .expect("buyer closes");
        restart_both(&mut buyer, &mut seller);
        observe_both(&mut buyer, &mut seller, &facts(3, &[]));
        assert_eq!(buyer.model.state(), closed(ClosureType::Cooperative));
        let buyers_key_share = buyer.model.my_private_key_share_for_peer_output().unwrap().clone();
        seller
            .model
            .close_with_key_share(&mut *seller.wallet, Ok(buyers_key_share), None)
            .expect("seller closes");
        restart_both(&mut buyer, &mut seller);
        assert_eq!(seller.model.state(), closed(ClosureType::Cooperative));

        // The protection after the close works after the restart: each trader answers a
        // warning of the peer with the penalty.
        let buyers_warning = buyer.model.my_signed_warning_tx().unwrap().clone();
        let warned = facts(WARNING_LOCK_TIME + 1, &[(&buyers_warning, 1)]);
        observe(&mut seller, &warned);
        assert_eq!(kinds(&seller), vec![TxKind::Penalty]);
    }
}

/// H2, H5: the traders restart in the states of the forced close, of the dispute path and of
/// the custom payout. After the restart the engines name the same transactions as before, and
/// events that happened while they did not run are found at the next look at the chain.
#[test]
fn h2_h5_restart_in_the_states_of_the_other_outcomes() {
    // Forced close: the seller restarts after the release and after his forced close; the buyer
    // restarts while the swap transaction is confirmed.
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    start_payment_and_release(&mut buyer, &mut seller);
    seller = restart(&seller);
    observe(&mut seller, &facts(3, &[]));
    seller.model.close_by_force().expect("after the restart");
    let swap_tx = tx_to_send(&seller, TxKind::Swap);
    seller = restart(&seller);
    observe(&mut seller, &facts(4, &[]));
    assert_eq!(seller.model.state(), closed(ClosureType::Forced));
    assert_eq!(tx_to_send(&seller, TxKind::Swap), swap_tx);
    buyer = restart(&buyer);
    assert_eq!(buyer.model.state(), TradeState::BuyerReadyToRelease);
    observe(&mut buyer, &facts(5, &[(&swap_tx, 1)]));
    assert_eq!(buyer.model.state(), closed(ClosureType::Forced));
    // The key share that the buyer read from the chain is in his record.
    buyer = restart(&buyer);
    assert_eq!(buyer.model.state(), closed(ClosureType::Forced));
    buyer
        .model
        .keys
        .my_payout_ctx()
        .aggregated_key()
        .unwrap()
        .prv_key()
        .expect("the buyer holds the key of his payout after the restart");

    // Dispute: the publisher restarts after his decision, the peer restarts before he sees the
    // warning. Redirect and claim are the same transactions as without a restart.
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[1]);
    let redirect_tx = seller.model.my_signed_redirect_tx().unwrap().clone();
    let claim_tx = buyer.model.my_claim_tx(FEE_RATE).unwrap();
    observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
    let warning_tx = buyer.model.start_warning().expect("lock time has passed");
    restart_both(&mut buyer, &mut seller);
    assert_eq!(buyer.model.state(), TradeState::BuyersWarning);
    observe_both(&mut buyer, &mut seller, &facts(WARNING_LOCK_TIME, &[]));
    assert_eq!(tx_to_send(&buyer, TxKind::Warning), warning_tx);
    let warned = facts(WARNING_LOCK_TIME + 10, &[(&warning_tx, 10)]);
    observe_both(&mut buyer, &mut seller, &warned);
    assert_eq!(tx_to_send(&seller, TxKind::Redirect), redirect_tx);
    assert_eq!(tx_to_send(&buyer, TxKind::Claim), claim_tx);

    // Custom payout: both restart after signing. The finalised transaction is the same.
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    for trader in [&mut buyer, &mut seller] {
        trader
            .model
            .sign_custom_payout(&mut *trader.wallet, Amount::from_sat(100_000), FEE_RATE)
            .expect("signed");
    }
    let buyers_psbt = buyer.model.custom_payout_psbt().unwrap().clone();
    restart_both(&mut buyer, &mut seller);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    assert_eq!(seller.model.state(), TradeState::CustomPayoutSigned);
    assert_eq!(*buyer.model.custom_payout_psbt().unwrap(), buyers_psbt);
    // A second request with other values is still refused after the restart.
    assert!(matches!(
        buyer
            .model
            .sign_custom_payout(&mut *buyer.wallet, Amount::from_sat(100_001), FEE_RATE),
        Err(ProtocolErrorKind::ConflictingRequest(_))
    ));
}

/// H5, A3: the deposit timeout is counted from a stored height, so it continues after a restart.
/// The deposit abort transaction is not stored. The engine builds it again, and it is the same
/// transaction.
#[test]
fn h5_deposit_timeout_continues_after_a_restart() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    let timeout = u32::from(config().deposit_timeout());
    observe_both(&mut buyer, &mut seller, &unconfirmed(0));
    restart_both(&mut buyer, &mut seller);
    assert_eq!(kinds(&seller), vec![], "the chain is not known yet");
    observe_both(&mut buyer, &mut seller, &unconfirmed(timeout));
    assert_eq!(kinds(&seller), vec![]);
    observe_both(&mut buyer, &mut seller, &unconfirmed(timeout + 1));
    for trader in [&mut buyer, &mut seller] {
        let abort_tx = tx_to_send(trader, TxKind::DepositAbort);
        *trader = restart(trader);
        assert_eq!(kinds(trader), vec![], "the chain is not known yet");
        observe(trader, &unconfirmed(timeout + 2));
        let rebuilt = tx_to_send(trader, TxKind::DepositAbort);
        assert_eq!(rebuilt.compute_txid(), abort_tx.compute_txid());
    }
}

/// H1, H8: a trade whose deposit transaction the trader has not signed is stored with its ID
/// and its reserved coins only. After a restart it is abandoned and the coins are released. Its
/// ID stays used.
#[test]
fn h1_trade_in_init_is_abandoned_after_a_restart() {
    let (mut me, peer) = tests::traders_before_deposit_signature(
        BUYER_SELLER_ROLES[0],
        tests::REDIRECTION_RECEIVERS,
        |_| {},
    );
    drop(peer);
    assert_eq!(me.model.state(), TradeState::Init);
    let reserved = me.model.reserved_coins().to_vec();
    assert!(!reserved.is_empty());
    let text = record_text(&me);
    assert!(text.contains("\"state\":\"Init\""), "{text}");
    assert!(text.contains("\"setup\":null"), "{text}");

    // The wallet survives the restart with its locks. The trade is loaded from its record.
    let record = me.model.to_record().unwrap();
    let mut loaded = TradeModel::from_record(&record, &*me.wallet).expect("loaded");
    assert_eq!(loaded.reserved_coins(), &reserved[..]);
    assert!(me.wallet.is_utxo_locked(reserved[0]));
    assert!(
        loaded
            .abandon_after_restart(&mut *me.wallet)
            .expect("abandoned")
    );
    assert_eq!(loaded.state(), closed(ClosureType::Abandoned));
    assert_eq!(me.wallet.list_locked_utxos(), vec![]);
    assert!(loaded.start_payment().is_err());

    // A trade after the deposit signature is not abandoned.
    let (mut buyer, _seller) = trade(BUYER_SELLER_ROLES[0]);
    assert!(
        !buyer
            .model
            .abandon_after_restart(&mut *buyer.wallet)
            .unwrap()
    );
    assert_eq!(buyer.model.state(), TradeState::Deposit);
}

/// H7: a record contains neither a secret key share of the trader nor a secret nonce. After
/// the close it contains the secret key share of the peer, which is why the store is encrypted.
#[test]
fn h7_record_holds_no_own_secret() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    let own_secrets = |trader: &Trader| {
        [
            &trader.model.keys.buyer_payout_ctx,
            &trader.model.keys.seller_payout_ctx,
        ]
        .map(|ctx| {
            ctx.my_key_share()
                .unwrap()
                .prv_key()
                .unwrap()
                .to_bytes()
                .to_lower_hex_string()
        })
    };
    let assert_no_own_secret = |trader: &Trader| {
        let text = record_text(trader);
        for secret in own_secrets(trader) {
            assert!(!text.contains(&secret), "own key share in the record");
        }
        assert!(!text.contains("sec_nonce") && !text.contains("secret_nonce"));
    };
    assert_no_own_secret(&buyer);
    assert_no_own_secret(&seller);
    assert!(record_text(&buyer).contains("\"secret_key_share\":null"));

    close_cooperatively(&mut buyer, &mut seller);
    assert_no_own_secret(&buyer);
    assert_no_own_secret(&seller);
    // The buyer's record holds the seller's key share of the buyer's payout.
    let sellers_secret = own_secrets(&seller)[0].clone();
    assert!(record_text(&buyer).contains(&sellers_secret));
}

/// The buffer of a record has its final size when it is created. A buffer that grows would
/// leave copies of the record, which holds the secret key share of the peer after a close, in
/// memory that is not wiped.
#[test]
fn record_buffer_does_not_grow() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    close_cooperatively(&mut buyer, &mut seller);
    for trader in [&buyer, &seller] {
        let record = trader.model.to_record().expect("record");
        assert_eq!(record.capacity(), record.len());
        let peers_key_share = trader
            .model
            .keys
            .my_payout_ctx()
            .peers_key_share()
            .unwrap()
            .prv_key()
            .expect("received at the close")
            .to_bytes()
            .to_lower_hex_string();
        assert!(record_text(trader).contains(&peers_key_share));
    }
}

/// H13: a trade that was loaded from its record cannot sign with a nonce of the setup again. The
/// secret nonces are not stored, so a signature for another message is impossible.
#[test]
fn h13_loaded_trade_cannot_sign_with_a_setup_nonce() {
    let (buyer, _seller) = trade(BUYER_SELLER_ROLES[0]);
    let mut loaded = restart(&buyer);
    let other_message = TapSighash::from_byte_array([7; 32]);
    for sig_ctx in loaded.model.all_sig_ctxs_mut() {
        assert!(matches!(
            sig_ctx.sign_partial(other_message),
            Err(crate::multisig::MultisigErrorKind::NonceReuse)
        ));
    }
}

/// H6: a record that is damaged, or whose data does not fit together, is refused as a whole.
#[test]
fn h6_damaged_record_is_refused() {
    let (mut buyer, mut seller) = trade(BUYER_SELLER_ROLES[0]);
    observe_both(&mut buyer, &mut seller, &facts(3, &[]));
    start_payment_and_release(&mut buyer, &mut seller);
    let text = record_text(&seller);
    let load = |text: &str| {
        TradeModel::from_record(text.as_bytes(), &mock_seller_trade_wallet())
            .map(|trade| trade.state())
    };
    assert_eq!(load(&text).unwrap(), TradeState::SellerReadyToRelease);

    let refused = |text: String, what: &str| match load(&text) {
        Err(
            ProtocolErrorKind::TradeRecord(_)
            | ProtocolErrorKind::Multisig(_)
            | ProtocolErrorKind::Transaction(_),
        ) => {}
        other => panic!("{what}: {other:?}"),
    };
    refused(text[..text.len() / 2].to_owned(), "cut off");
    refused(
        text.replace("\"version\":1", "\"version\":2"),
        "other version",
    );
    refused(
        text.replace("SellerReadyToRelease", "ReadyToRelease"),
        "unknown state",
    );
    // Another amount gives other transactions, for which the stored signatures are not valid.
    refused(
        text.replace("\"trade_amount\":200000", "\"trade_amount\":200001"),
        "changed amount",
    );
    // A partial signature of the peer that was changed in one digit.
    let field = "\"redirect_tx_signature\":\"";
    let position = text.find(field).expect("field") + field.len();
    let mut changed = text.clone();
    let digit = if &text[position..=position] == "0" {
        "1"
    } else {
        "0"
    };
    changed.replace_range(position..=position, digit);
    refused(changed, "changed signature");
    // A record of the buyer with the wallet of the seller: other key shares.
    let buyers_text = record_text(&buyer);
    refused(buyers_text, "record of another wallet");
}

/// The ID of a trade is part of its record, and the key shares follow from the ID and the seed
/// (D10): a record under another ID does not load.
#[test]
fn record_belongs_to_one_trade_id() {
    let (buyer, _seller) = trade(BUYER_SELLER_ROLES[0]);
    let text = record_text(&buyer);
    let other = new_trader_for_trade(Role::BuyerAsTaker, "other-trade");
    assert_ne!(other.model.trade_id(), buyer.model.trade_id());
    let renamed = text.replace("\"trade_id\":\"trade\"", "\"trade_id\":\"other-trade\"");
    assert_ne!(renamed, text);
    assert!(TradeModel::from_record(renamed.as_bytes(), &mock_buyer_trade_wallet()).is_err());
}
