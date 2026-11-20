//! The steps of a trade between a seller and a buyer, through the trade services of their apps.
//!
//! The test code carries the messages of the trade protocol from one trader to the other, as
//! bisq2 does. The messages A to G are defined in `docs/specifications/trade`. In the trades of
//! this module the seller made the offer and the buyer took it, so the buyer is the taker: he
//! starts the setup and publishes the deposit transaction.

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Amount, FeeRate, Txid};
use futures_util::StreamExt as _;
use protocol::params::MIN_CONFIRMATIONS;
use protocol::receiver::Receiver;
use rpc::pb::musigrpc::{
    CloseTradeRequest, ClosureKind, DepositPsbt, DepositTxSignatureRequest, NonceSharesMessage,
    NonceSharesRequest, PartialSignaturesMessage, PartialSignaturesRequest, PubKeySharesRequest,
    PubKeySharesResponse, PublishDepositTxRequest, ReceiverAddressAndAmount, Role,
    SubscribeTradeEventsRequest, SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest,
    TradeStateKind, TxConfirmationStatus,
};
use tonic::Streaming;

use crate::setup::{User, WAIT};

/// The fee rate of the deposit transaction and of the prepared transactions of a trade: 10 sat
/// per vbyte.
const FEE_RATE: FeeRate = FeeRate::from_sat_per_kwu(2_500);

/// The amounts that the two traders agreed on.
pub struct Terms {
    /// The amount that the buyer buys.
    pub trade_amount: Amount,
    pub buyers_security_deposit: Amount,
    pub sellers_security_deposit: Amount,
}

/// The deposit transaction of a trade that both traders have signed and that is not published.
pub struct SignedDeposit {
    pub txid: Txid,
    /// The deposit transaction with the signatures of the seller. The buyer needs it to publish.
    sellers_psbt: DepositPsbt,
}

/// The reports of both apps about the confirmations of the deposit transaction.
pub struct DepositConfirmations {
    sellers: Streaming<TxConfirmationStatus>,
    buyers: Streaming<TxConfirmationStatus>,
}

/// Sets up a trade: the messages A to D. Afterwards both traders have signed the deposit
/// transaction, and their wallets have reserved the coins that it spends.
pub async fn set_up(
    seller: &mut User,
    buyer: &mut User,
    trade_id: &str,
    terms: &Terms,
) -> anyhow::Result<SignedDeposit> {
    let init = |my_role: Role| PubKeySharesRequest {
        trade_id: trade_id.to_owned(),
        my_role: my_role.into(),
    };
    let nonce_request = |peers_keys: PubKeySharesResponse| NonceSharesRequest {
        trade_id: trade_id.to_owned(),
        buyer_output_peers_pub_key_share: peers_keys.buyer_output_pub_key_share,
        seller_output_peers_pub_key_share: peers_keys.seller_output_pub_key_share,
        peers_multisig_script_key: peers_keys.multisig_script_key,
        deposit_tx_fee_rate: FEE_RATE.to_sat_per_kwu(),
        prepared_tx_fee_rate: FEE_RATE.to_sat_per_kwu(),
        trade_amount: terms.trade_amount.to_sat(),
        buyers_security_deposit: terms.buyers_security_deposit.to_sat(),
        sellers_security_deposit: terms.sellers_security_deposit.to_sat(),
        trade_fee_receiver: None,
    };

    // Message A: the key shares of the taker.
    let buyers_keys = buyer
        .musig
        .init_trade(init(Role::BuyerAsTaker))
        .await?
        .into_inner();
    // Message B: the key shares of the maker, his half of the deposit and his nonces.
    let sellers_keys = seller
        .musig
        .init_trade(init(Role::SellerAsMaker))
        .await?
        .into_inner();
    let sellers_nonces = seller
        .musig
        .get_nonce_shares(nonce_request(buyers_keys))
        .await?
        .into_inner();
    // Message C: the half of the deposit of the taker, his nonces and his partial signatures.
    let buyers_nonces = buyer
        .musig
        .get_nonce_shares(nonce_request(sellers_keys))
        .await?
        .into_inner();
    anyhow::ensure!(
        buyers_nonces.redirection_amount_msat == sellers_nonces.redirection_amount_msat,
        "the traders report different amounts for the redirect transactions"
    );
    // The receivers of the redirect transactions are part of the contract. In this trade one
    // receiver gets everything. An address of a node stands for it.
    let receivers = Receiver::compute_receivers_from_shares(
        vec![(seller.node.new_address()?, 1.0)],
        buyers_nonces.redirection_amount_msat,
        FEE_RATE,
    )
    .context("no receiver of the redirect transactions")?;
    let redirection_receivers: Vec<_> = receivers
        .iter()
        .map(|receiver| ReceiverAddressAndAmount {
            address: receiver.address.to_string(),
            amount: receiver.amount.to_sat(),
        })
        .collect();
    let signatures_request = |peers_nonces: NonceSharesMessage| PartialSignaturesRequest {
        trade_id: trade_id.to_owned(),
        peers_nonce_shares: Some(peers_nonces),
        redirection_receivers: redirection_receivers.clone(),
        buyer_ready_to_release: false,
    };
    let buyers_signatures = buyer
        .musig
        .get_partial_signatures(signatures_request(sellers_nonces))
        .await?
        .into_inner();
    // Message D: the partial signatures of the maker, and the deposit transaction with his
    // signatures.
    let sellers_signatures = seller
        .musig
        .get_partial_signatures(signatures_request(buyers_nonces))
        .await?
        .into_inner();
    // The IDs of the deposit, warning and redirect transactions identify the contract.
    let contract = buyers_signatures
        .contractual_tx_ids
        .clone()
        .context("the buyer reports no transaction IDs")?;
    anyhow::ensure!(
        sellers_signatures.contractual_tx_ids.as_ref() == Some(&contract),
        "the traders report different transactions of the trade"
    );
    let deposit_request = |peers_signatures: PartialSignaturesMessage| DepositTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        peers_partial_signatures: Some(peers_signatures),
    };
    let sellers_psbt = seller
        .musig
        .sign_deposit_tx(deposit_request(buyers_signatures))
        .await?
        .into_inner();
    buyer
        .musig
        .sign_deposit_tx(deposit_request(sellers_signatures))
        .await?;

    Ok(SignedDeposit {
        txid: contract.deposit_tx_id.parse()?,
        sellers_psbt,
    })
}

/// The buyer publishes the deposit transaction. Both apps report its confirmations from now on.
pub async fn publish_deposit(
    seller: &mut User,
    buyer: &mut User,
    trade_id: &str,
    deposit: SignedDeposit,
) -> anyhow::Result<DepositConfirmations> {
    let sellers = seller
        .musig
        .subscribe_tx_confirmation_status(SubscribeTxConfirmationStatusRequest {
            trade_id: trade_id.to_owned(),
        })
        .await?
        .into_inner();
    let buyers = buyer
        .musig
        .publish_deposit_tx(PublishDepositTxRequest {
            trade_id: trade_id.to_owned(),
            peers_deposit_psbt: Some(deposit.sellers_psbt),
        })
        .await?
        .into_inner();
    Ok(DepositConfirmations { sellers, buyers })
}

impl DepositConfirmations {
    /// Waits until both apps have reported that the deposit transaction counts as confirmed. An
    /// app reports the confirmations up to [`MIN_CONFIRMATIONS`] and then ends its reports.
    pub async fn wait_until_confirmed(mut self) -> anyhow::Result<()> {
        for (trader, reports) in [("seller", &mut self.sellers), ("buyer", &mut self.buyers)] {
            let mut confirmations = 0;
            while let Some(report) = tokio::time::timeout(WAIT, reports.next())
                .await
                .with_context(|| format!("the app of the {trader} reports no confirmation"))?
            {
                confirmations = report?.num_confirmations;
            }
            anyhow::ensure!(
                confirmations == u32::from(MIN_CONFIRMATIONS),
                "the reports of the {trader} end at {confirmations} confirmations"
            );
        }
        Ok(())
    }
}

/// Closes the trade cooperatively: the messages E, F and G. The deposit transaction must count as
/// confirmed. Afterwards each trader owns his payout in an output of the deposit transaction.
pub async fn close_cooperatively(
    seller: &mut User,
    buyer: &mut User,
    trade_id: &str,
) -> anyhow::Result<()> {
    let swap_request = |signature: Vec<u8>, seller_ready_to_release| SwapTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        swap_tx_input_peers_partial_signature: signature,
        seller_ready_to_release,
    };
    let close_request = |key_share: Option<Vec<u8>>| CloseTradeRequest {
        trade_id: trade_id.to_owned(),
        my_output_peers_prv_key_share: key_share,
        ..CloseTradeRequest::default()
    };

    // Message E: the buyer has started the payment.
    let message_e = buyer
        .musig
        .get_partial_signatures(PartialSignaturesRequest {
            trade_id: trade_id.to_owned(),
            buyer_ready_to_release: true,
            ..PartialSignaturesRequest::default()
        })
        .await?
        .into_inner();
    let buyers_swap_signature = message_e
        .swap_tx_input_partial_signature
        .context("message E contains no signature")?;
    seller
        .musig
        .sign_swap_tx(swap_request(buyers_swap_signature, false))
        .await?;
    // Message F: the seller has received the payment and releases the payout of the buyer.
    let message_f = seller
        .musig
        .sign_swap_tx(swap_request(Vec::new(), true))
        .await?
        .into_inner();
    // Message G: the buyer releases the payout of the seller.
    let message_g = buyer
        .musig
        .close_trade(close_request(message_f.peer_output_prv_key_share))
        .await?
        .into_inner();
    seller
        .musig
        .close_trade(close_request(message_g.peer_output_prv_key_share))
        .await?;
    Ok(())
}

/// How the trade was closed, as the app of `user` reports it, or `None` if the trade is not
/// closed.
pub async fn closure(user: &mut User, trade_id: &str) -> anyhow::Result<Option<ClosureKind>> {
    let request = SubscribeTradeEventsRequest {
        trade_id: trade_id.to_owned(),
    };
    let mut events = user
        .musig
        .subscribe_trade_events(request)
        .await?
        .into_inner();
    // The first event of the stream reports the state of the trade.
    let state = tokio::time::timeout(WAIT, events.next())
        .await?
        .context("the app reports no state of the trade")??;
    Ok((state.state() == TradeStateKind::TradeClosed).then(|| state.closure_type()))
}
