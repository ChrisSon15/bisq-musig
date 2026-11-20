//! The steps of a trade between a seller and a buyer, through the trade services of their
//! `musigd` processes.
//!
//! The test code carries the messages of the trade protocol from one trader to the other, as
//! bisq2 does. The messages A to G are defined in `docs/specifications/trade`. In the trades of
//! this module the seller made the offer and the buyer took it, so the buyer is the taker: he
//! starts the setup and publishes the deposit transaction.
//!
//! Every step takes the client of the seller before the client of the buyer.

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Txid};
use futures_util::StreamExt as _;
use protocol::receiver::Receiver;
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{
    CloseTradeRequest, DepositPsbt, DepositTxSignatureRequest, NonceSharesMessage,
    NonceSharesRequest, PartialSignaturesMessage, PartialSignaturesRequest, PubKeySharesRequest,
    PubKeySharesResponse, PublishDepositTxRequest, ReceiverAddressAndAmount, Role,
    SubscribeTradeEventsRequest, SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest,
    TradeEvent, TradeEventType, TxConfirmationStatus,
};
use tonic::Streaming;
use tonic::transport::Channel;

use super::musigd::WAIT;

/// The fee rate of the deposit transaction and of the prepared transactions of a trade: 10 sat
/// per vbyte.
pub const FEE_RATE: FeeRate = FeeRate::from_sat_per_kwu(2_500);

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
    pub sellers_psbt: DepositPsbt,
}

/// The reports of both `musigd` processes about the confirmations of the deposit transaction.
pub struct DepositConfirmations {
    pub sellers: Streaming<TxConfirmationStatus>,
    pub buyers: Streaming<TxConfirmationStatus>,
}

/// The request that creates the trade for a trader with the role `my_role`.
pub fn init_request(trade_id: &str, my_role: Role) -> PubKeySharesRequest {
    PubKeySharesRequest {
        trade_id: trade_id.to_owned(),
        my_role: my_role.into(),
    }
}

/// The request with the terms of the trade and the key shares of the peer. The trader answers
/// with his half of the deposit and his nonces, and his wallet reserves the coins of the half.
pub fn nonce_request(
    trade_id: &str,
    terms: &Terms,
    peers_keys: PubKeySharesResponse,
) -> NonceSharesRequest {
    NonceSharesRequest {
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
    }
}

/// The request of the buyer for message E: he has started the payment.
pub fn start_payment_request(trade_id: &str) -> PartialSignaturesRequest {
    PartialSignaturesRequest {
        trade_id: trade_id.to_owned(),
        buyer_ready_to_release: true,
        ..PartialSignaturesRequest::default()
    }
}

/// A request of the seller about the swap transaction. With the signature of message E the
/// seller takes this message. With `seller_ready_to_release` he releases the payout of the
/// buyer, and the response is message F.
pub fn swap_request(
    trade_id: &str,
    buyers_signature: Vec<u8>,
    seller_ready_to_release: bool,
) -> SwapTxSignatureRequest {
    SwapTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        swap_tx_input_peers_partial_signature: buyers_signature,
        seller_ready_to_release,
    }
}

/// The request that closes the trade of a trader with the key share that the peer released.
pub fn close_request(trade_id: &str, peers_key_share: Option<Vec<u8>>) -> CloseTradeRequest {
    CloseTradeRequest {
        trade_id: trade_id.to_owned(),
        my_output_peers_prv_key_share: peers_key_share,
        ..CloseTradeRequest::default()
    }
}

/// Sets up a trade: the messages A to D. Afterwards both traders have signed the deposit
/// transaction, and their wallets have reserved the coins that it spends. The receivers of the
/// redirect transactions are part of the contract. In this trade the one receiver
/// `redirect_receiver` gets everything.
pub async fn set_up(
    seller: &mut MusigClient<Channel>,
    buyer: &mut MusigClient<Channel>,
    trade_id: &str,
    terms: &Terms,
    redirect_receiver: Address,
) -> anyhow::Result<SignedDeposit> {
    // Message A: the key shares of the taker.
    let buyers_keys = buyer
        .init_trade(init_request(trade_id, Role::BuyerAsTaker))
        .await?
        .into_inner();
    // Message B: the key shares of the maker, his half of the deposit and his nonces.
    let sellers_keys = seller
        .init_trade(init_request(trade_id, Role::SellerAsMaker))
        .await?
        .into_inner();
    let sellers_nonces = seller
        .get_nonce_shares(nonce_request(trade_id, terms, buyers_keys))
        .await?
        .into_inner();
    // Message C: the half of the deposit of the taker, his nonces and his partial signatures.
    let buyers_nonces = buyer
        .get_nonce_shares(nonce_request(trade_id, terms, sellers_keys))
        .await?
        .into_inner();
    anyhow::ensure!(
        buyers_nonces.redirection_amount_msat == sellers_nonces.redirection_amount_msat,
        "the traders report different amounts for the redirect transactions"
    );
    let receivers = Receiver::compute_receivers_from_shares(
        vec![(redirect_receiver, 1.0)],
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
        .get_partial_signatures(signatures_request(sellers_nonces))
        .await?
        .into_inner();
    // Message D: the partial signatures of the maker, and the deposit transaction with his
    // signatures.
    let sellers_signatures = seller
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
        .sign_deposit_tx(deposit_request(buyers_signatures))
        .await?
        .into_inner();
    buyer
        .sign_deposit_tx(deposit_request(sellers_signatures))
        .await?;

    Ok(SignedDeposit {
        txid: contract.deposit_tx_id.parse()?,
        sellers_psbt,
    })
}

/// The buyer publishes the deposit transaction. Both processes report its confirmations from now
/// on: the seller, who is the maker, observes the deposit.
pub async fn publish_deposit(
    seller: &mut MusigClient<Channel>,
    buyer: &mut MusigClient<Channel>,
    trade_id: &str,
    deposit: SignedDeposit,
) -> anyhow::Result<DepositConfirmations> {
    let sellers = seller
        .subscribe_tx_confirmation_status(SubscribeTxConfirmationStatusRequest {
            trade_id: trade_id.to_owned(),
        })
        .await?
        .into_inner();
    let buyers = buyer
        .publish_deposit_tx(PublishDepositTxRequest {
            trade_id: trade_id.to_owned(),
            peers_deposit_psbt: Some(deposit.sellers_psbt),
        })
        .await?
        .into_inner();
    Ok(DepositConfirmations { sellers, buyers })
}

/// Message E: the buyer has started the payment, and the seller takes the message. The deposit
/// transaction must count as confirmed.
pub async fn start_payment(
    seller: &mut MusigClient<Channel>,
    buyer: &mut MusigClient<Channel>,
    trade_id: &str,
) -> anyhow::Result<()> {
    let message_e = buyer
        .get_partial_signatures(start_payment_request(trade_id))
        .await?
        .into_inner();
    let buyers_signature = message_e
        .swap_tx_input_partial_signature
        .context("message E contains no signature")?;
    seller
        .sign_swap_tx(swap_request(trade_id, buyers_signature, false))
        .await?;
    Ok(())
}

/// Message F: the seller has received the payment and releases the payout of the buyer. Returns
/// the key share of the seller for the payout of the buyer.
pub async fn release(
    seller: &mut MusigClient<Channel>,
    trade_id: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let message_f = seller
        .sign_swap_tx(swap_request(trade_id, Vec::new(), true))
        .await?
        .into_inner();
    Ok(message_f.peer_output_prv_key_share)
}

/// Closes the trade cooperatively: the messages E, F and G. The deposit transaction must count as
/// confirmed. Afterwards each trader owns his payout in an output of the deposit transaction.
pub async fn close_cooperatively(
    seller: &mut MusigClient<Channel>,
    buyer: &mut MusigClient<Channel>,
    trade_id: &str,
) -> anyhow::Result<()> {
    start_payment(seller, buyer, trade_id).await?;
    let sellers_key_share = release(seller, trade_id).await?;
    // Message G: the buyer releases the payout of the seller.
    let message_g = buyer
        .close_trade(close_request(trade_id, sellers_key_share))
        .await?
        .into_inner();
    seller
        .close_trade(close_request(trade_id, message_g.peer_output_prv_key_share))
        .await?;
    Ok(())
}

/// The event stream of a trade.
pub async fn trade_events(
    musig: &mut MusigClient<Channel>,
    trade_id: &str,
) -> anyhow::Result<Streaming<TradeEvent>> {
    let request = SubscribeTradeEventsRequest {
        trade_id: trade_id.to_owned(),
    };
    Ok(musig.subscribe_trade_events(request).await?.into_inner())
}

/// The current state of a trade: the first event of a new event stream of the trade.
pub async fn trade_state(
    musig: &mut MusigClient<Channel>,
    trade_id: &str,
) -> anyhow::Result<TradeEvent> {
    let mut events = trade_events(musig, trade_id).await?;
    let first = tokio::time::timeout(WAIT, events.next())
        .await?
        .context("the stream of the trade has no event")??;
    anyhow::ensure!(
        first.r#type() == TradeEventType::State,
        "the stream of the trade starts with {:?}",
        first.r#type()
    );
    Ok(first)
}
