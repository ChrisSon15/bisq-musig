use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::marker::Send;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bdk_wallet::bitcoin::{Amount, FeeRate, Psbt, Transaction, Txid, consensus};
use bdk_wallet::serde_json;
use chain::client::{ChainAccess, ClientError, PeerProblem};
use drop_stream::DropStreamExt as _;
use futures_util::stream::{self, BoxStream, Stream, StreamExt as _, TryStream, TryStreamExt as _};
use musig2::PartialSignature;
use protocol::chain_facts::{
    ChainFacts, DeadlineKind, TradeEvent as EngineEvent, TxKind, TxToSend, UserAction,
};
use protocol::params::{EngineConfig, TradeParams, engine_fee_rate};
use protocol::state::{ClosureType, TradeState};
use protocol::storage::ByVal;
use protocol::trade_model::{
    ExchangedKeys, ExchangedSigs, PeerData, ProtocolErrorKind, TradeModel, TradeModelStore,
    UndecodablePeerData,
};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, broadcast};
use tonic::metadata::MetadataValue;
use tonic::{Request, Response, Result, Status};
use tracing::{Span, debug, error, info, instrument, trace, warn};
use wallet::protocol_wallet_api::ProtocolWalletApi;
use wallet::secret::SecretScalar;
use zeroize::Zeroizing;

use crate::pb::convert::{CheckInSignedRange as _, TryProtoInto};
pub use crate::pb::musigrpc::musig_server::MusigServer;
use crate::pb::musigrpc::{
    AbandonTradeRequest, AbandonTradeResponse, CloseTradeRequest, CloseTradeResponse, ClosureKind,
    CustomCloseTradeRequest, CustomCloseTradeResponse, CustomPayoutPsbt, CustomPayoutPsbtRequest,
    DepositPsbt, DepositTxSignatureRequest, NonceSharesMessage, NonceSharesRequest,
    PartialSignaturesMessage, PartialSignaturesRequest, PubKeySharesRequest, PubKeySharesResponse,
    PublishDepositTxRequest, PublishWarningTxRequest, PublishWarningTxResponse,
    SubscribeTradeEventsRequest, SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest,
    SwapTxSignatureResponse, TradeDeadline, TradeDeadlineKind, TradeEvent, TradeEventType,
    TradeStateKind, TradeTxKind, TradeUserAction, TxConfirmationStatus, musig_server,
};

/// What an error of the trade service means for the caller. Every error status carries its case
/// in its metadata, so that the caller does not have to read the text of the message, and does
/// not depend on the gRPC status code, which several cases share.
#[expect(
    clippy::exhaustive_enums,
    reason = "the cases are the cases of the specification, a closed set"
)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ErrorCase {
    /// A precondition of the request does not hold (role, state, order), or data of the caller
    /// is invalid. The trade continues. The same request does not succeed in the same state.
    Refused,
    /// A condition of the chain does not hold yet: too few confirmations, a lock time, a margin.
    /// The trade continues. The same request can succeed after further blocks.
    RefusedForNow,
    /// The peer sent data that an unmodified client never produces, now or earlier. The trade
    /// is terminated.
    Terminated,
    /// No trade with this ID exists.
    UnknownTrade,
    /// A trade with this ID exists or existed. A new trade needs another ID.
    TradeIdUsed,
    /// The service cannot process the request for a reason outside of the trade: no wallet is
    /// open, no peer of the network is reachable, or the trade could not be stored. The trade
    /// is as before. The same request can succeed later.
    Temporary,
    /// The trades of the wallet could not be loaded. No request of a trade is served.
    TradesNotLoaded,
    /// A defect of the service. Whether a repetition helps is not known.
    Internal,
}

impl ErrorCase {
    /// The metadata key of the case of an error.
    pub const KEY: &'static str = "musig-error-case";
    /// The metadata key that says whether the same request can succeed later: `later`, `never`
    /// or `unknown`.
    pub const RETRY_KEY: &'static str = "musig-error-retry";

    /// The value of the case in the metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::RefusedForNow => "refused-for-now",
            Self::Terminated => "terminated",
            Self::UnknownTrade => "unknown-trade",
            Self::TradeIdUsed => "trade-id-used",
            Self::Temporary => "temporary",
            Self::TradesNotLoaded => "trades-not-loaded",
            Self::Internal => "internal",
        }
    }

    /// Whether the same request can succeed when it is sent again later, without another
    /// request in between.
    pub const fn retry(self) -> &'static str {
        match self {
            Self::RefusedForNow | Self::Temporary => "later",
            Self::Refused
            | Self::Terminated
            | Self::UnknownTrade
            | Self::TradeIdUsed
            | Self::TradesNotLoaded => "never",
            Self::Internal => "unknown",
        }
    }

    /// Marks a status with this case.
    fn of(self, mut status: Status) -> Status {
        let metadata = status.metadata_mut();
        metadata.insert(Self::KEY, MetadataValue::from_static(self.name()));
        metadata.insert(Self::RETRY_KEY, MetadataValue::from_static(self.retry()));
        status
    }

    /// The case of a status that was created without one: an error of the conversion of a
    /// request, which concerns the data of the caller.
    fn of_unmarked(status: Status) -> Status {
        if status.metadata().contains_key(Self::KEY) {
            return status;
        }
        let case = match status.code() {
            tonic::Code::InvalidArgument | tonic::Code::OutOfRange | tonic::Code::NotFound => {
                Self::Refused
            }
            _ => Self::Internal,
        };
        case.of(status)
    }
}

/// An operation of the trade service that needs the wallet of the trader.
pub type WalletOperation<'a> = dyn for<'w> FnMut(&'w mut (dyn ProtocolWalletApi + 'w)) + Send + 'a;

/// Gives the trade service access to the wallet of the trader.
#[tonic::async_trait]
pub trait TradeWalletProvider: Send + Sync {
    /// Runs `operation` with exclusive access to the wallet. If no wallet is open, `operation` is
    /// not run and the result is `false`.
    async fn with_trade_wallet(&self, operation: &mut WalletOperation<'_>) -> bool;
}

/// A wallet provider without a wallet. The trade service of a `musigd` without a wallet
/// directory uses it, and refuses every request that needs a wallet.
#[derive(Debug, Default)]
pub struct NoTradeWallet;

#[tonic::async_trait]
impl TradeWalletProvider for NoTradeWallet {
    async fn with_trade_wallet(&self, _operation: &mut WalletOperation<'_>) -> bool {
        false
    }
}

/// The service `musigrpc.Musig`. It converts between the protobuf messages and the operations of
/// the trade engine. The trades, the wallet and the configuration of the engine are supplied by
/// the caller.
pub struct MusigImpl {
    trades: Arc<dyn TradeModelStore>,
    wallet: Arc<dyn TradeWalletProvider>,
    config: EngineConfig,
    /// The access to the chain. Without it the service can set up a trade, but it cannot
    /// publish or observe the deposit transaction.
    chain: Option<Arc<dyn ChainAccess>>,
    /// The events of all trades, for the streams of `SubscribeTradeEvents`.
    events: broadcast::Sender<TradeEvent>,
    /// Wakes the task that watches the trades, after a request has changed a trade.
    wake_watcher: Arc<Notify>,
    /// The IDs of the trades whose outputs are watched on the chain in this run of the program.
    watched: Arc<Mutex<HashSet<String>>>,
    /// The task that watches the trades. It ends with the service.
    watcher: Option<tokio::task::AbortHandle>,
}

impl Drop for MusigImpl {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

/// The number of trade events that a slow subscriber can fall behind before it loses events.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

impl MusigImpl {
    pub fn new(
        trades: Arc<dyn TradeModelStore>,
        wallet: Arc<dyn TradeWalletProvider>,
        config: EngineConfig,
    ) -> Self {
        Self {
            trades,
            wallet,
            config,
            chain: None,
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            wake_watcher: Arc::new(Notify::new()),
            watched: Arc::new(Mutex::new(HashSet::new())),
            watcher: None,
        }
    }

    /// Supplies the access to the chain, and starts the task that watches the trades on the
    /// chain and sends their transactions.
    ///
    /// # Panics
    /// If it is called outside of a Tokio runtime.
    #[must_use]
    pub fn with_chain(mut self, chain: Arc<dyn ChainAccess>) -> Self {
        let watcher = tokio::spawn(watch_trades(
            Arc::clone(&self.trades),
            Arc::clone(&self.wallet),
            Arc::clone(&chain),
            self.events.clone(),
            Arc::clone(&self.wake_watcher),
            Arc::clone(&self.watched),
        ));
        self.watcher = Some(watcher.abort_handle());
        self.chain = Some(chain);
        self
    }

    /// The access to the chain, or the status for a request that needs it.
    fn chain(&self) -> Result<Arc<dyn ChainAccess>> {
        self.chain.clone().ok_or_else(|| {
            ErrorCase::Temporary.of(Status::failed_precondition(
                "the trade service has no access to the chain; start musigd with a peer",
            ))
        })
    }

    /// The access to the chain for a request that changes the trade and sends a transaction.
    /// While a problem with the peers lasts, nothing can be sent, so such a request does not
    /// proceed and changes nothing. The caller can repeat it.
    ///
    /// A request that only observes the chain does not use this check: it is accepted, and its
    /// events arrive when a peer is reachable again (decision D21 of the plan).
    fn chain_for_sending(&self) -> Result<Arc<dyn ChainAccess>> {
        let chain = self.chain()?;
        if let Some(problem) = chain.peer_problem() {
            return Err(ErrorCase::Temporary.of(Status::unavailable(format!(
                "nothing can be sent at the moment: {problem}"
            ))));
        }
        Ok(chain)
    }

    /// The status of every trade request while the stored trades could not be loaded.
    fn check_trade_store(&self) -> Result<()> {
        match self.trades.unusable() {
            Some(reason) => Err(ErrorCase::TradesNotLoaded.of(Status::data_loss(format!(
                "the trades of the wallet could not be loaded, so no trade request is served: \
                 {reason}"
            )))),
            None => Ok(()),
        }
    }

    /// Runs `operation` with the wallet of the trader, or returns an error if no wallet is open.
    async fn with_wallet<T: Send>(
        &self,
        operation: impl FnOnce(&mut dyn ProtocolWalletApi) -> Result<T> + Send,
    ) -> Result<T> {
        let mut operation = Some(operation);
        let mut result = None;
        self.wallet
            .with_trade_wallet(&mut |wallet| {
                if let Some(operation) = operation.take() {
                    result = Some(operation(wallet));
                }
            })
            .await;
        result.unwrap_or_else(|| {
            Err(ErrorCase::Temporary.of(Status::failed_precondition(
                "no wallet is open; the trade service needs the wallet",
            )))
        })
    }

    /// Looks up the trade of the request and runs `handler` with it and with the wallet of the
    /// trader. Every request of a trade needs the open wallet: the engine takes keys and coins
    /// from it, records transactions in it, and releases reserved coins in it.
    async fn handle_trade_request<Req, Res, F>(
        &self,
        request: Request<Req>,
        handler: F,
    ) -> Result<Response<Res>>
    where
        Req: MusigRequest + Send,
        Res: Serialize + Send,
        F: FnOnce(Req, &mut TradeModel, &mut dyn ProtocolWalletApi) -> Result<Res, HandlerError>
            + Send,
    {
        if let Some(chain) = &self.chain {
            wait_until_evaluated(&**chain).await;
        }
        self.with_wallet(|wallet| {
            handle_request(request, move |request| {
                self.check_trade_store()?;
                let trade_model =
                    self.trades
                        .get_trade_model(request.trade_id())
                        .ok_or_else(|| {
                            ErrorCase::UnknownTrade.of(Status::not_found(format!(
                                "missing trade with id: {}",
                                request.trade_id()
                            )))
                        })?;
                let mut trade_model = trade_model.lock().unwrap();
                // The engine checks the chain conditions of a request against the facts that it
                // was told last, so it is told the current facts first.
                if let Some(chain) = &self.chain {
                    ensure_watched(&**chain, &self.watched, &trade_model, wallet);
                    observe_chain(&**chain, &mut trade_model, wallet, &self.events);
                }
                let status_before = trade_model.status();
                let result = handler(request, &mut trade_model, wallet);
                if let Some(chain) = &self.chain {
                    // From the moment the trader has signed the deposit transaction, the peer
                    // can publish it.
                    ensure_watched(&**chain, &self.watched, &trade_model, wallet);
                }
                // A request can change the state, and without a change of the state the
                // deadlines, the required actions and the terminated mark.
                let status = trade_model.status();
                let event_type = if status.state != status_before.state {
                    Some(TradeEventType::State)
                } else if status != status_before {
                    Some(TradeEventType::Status)
                } else {
                    None
                };
                if let Some(event_type) = event_type {
                    let event = trade_event(&trade_model, self.chain.as_deref(), event_type);
                    let _ = self.events.send(event);
                }
                // A request can give the trade a transaction to send.
                self.wake_watcher.notify_one();
                // A setup that the engine terminated in this request is abandoned, which
                // releases its reserved coins.
                if let Err(error) = trade_model.abandon_terminated_setup(wallet) {
                    error!(
                        trade_id = trade_model.trade_id(),
                        %error,
                        "Could not release the reserved coins of a terminated trade."
                    );
                }
                // The new state is on disk before the response leaves the service, also when
                // the request was refused: a refusal can have terminated the trade.
                store_trade(&trade_model, wallet)?;
                result
            })
        })
        .await
    }
}

#[tonic::async_trait]
impl musig_server::Musig for MusigImpl {
    #[instrument(skip_all)]
    async fn init_trade(
        &self,
        request: Request<PubKeySharesRequest>,
    ) -> Result<Response<PubKeySharesResponse>> {
        self.with_wallet(|wallet| {
            handle_request(request, |request| {
                self.check_trade_store()?;
                if self.trades.contains_trade(&request.trade_id) {
                    return Err(ProtocolErrorKind::DuplicateTradeId(request.trade_id).into());
                }
                let role = request.my_role.try_proto_into()?;
                let trade_model = TradeModel::new(request.trade_id, role, wallet)?;
                let my_key_shares = trade_model.my_key_shares()?;
                let response = PubKeySharesResponse {
                    buyer_output_pub_key_share: my_key_shares.buyer_payout.serialize().into(),
                    seller_output_pub_key_share: my_key_shares.seller_payout.serialize().into(),
                    multisig_script_key: my_key_shares.multisig_script.serialize().into(),
                    // The height 0 means that the chain is not known (yet).
                    current_block_height: self
                        .chain
                        .as_ref()
                        .and_then(|chain| chain.tip_height())
                        .unwrap_or(0),
                };
                // The ID of a trade is stored from its first request on, so that it is never used
                // for a second trade, also after a restart.
                store_trade(&trade_model, wallet)?;
                self.trades.add_trade_model(trade_model)?;

                Ok(response)
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_nonce_shares(
        &self,
        request: Request<NonceSharesRequest>,
    ) -> Result<Response<NonceSharesMessage>> {
        self.handle_trade_request(request, |request, trade_model, wallet| {
            let peers_key_shares = (|| {
                Ok(ExchangedKeys {
                    buyer_payout: request.buyer_output_peers_pub_key_share.try_proto_into()?,
                    seller_payout: request.seller_output_peers_pub_key_share.try_proto_into()?,
                    multisig_script: request.peers_multisig_script_key.try_proto_into()?,
                })
            })()
            .peer_data();
            let params = TradeParams {
                trade_amount: Amount::from_sat(request.trade_amount.check_in_signed_range()?),
                buyers_security_deposit: Amount::from_sat(
                    request.buyers_security_deposit.check_in_signed_range()?,
                ),
                sellers_security_deposit: Amount::from_sat(
                    request.sellers_security_deposit.check_in_signed_range()?,
                ),
                deposit_tx_fee_rate: FeeRate::from_sat_per_kwu(
                    request.deposit_tx_fee_rate.check_in_signed_range()?,
                ),
                prepared_tx_fee_rate: FeeRate::from_sat_per_kwu(
                    request.prepared_tx_fee_rate.check_in_signed_range()?,
                ),
                trade_fee_receiver: request.trade_fee_receiver.try_proto_into()?,
            };
            trade_model.receive_key_shares(wallet, &self.config, peers_key_shares, params)?;

            let redirection_amount_msat = trade_model
                .redirection_amount_msat()?
                .check_in_signed_range()?;
            let my_addresses = trade_model.my_addresses()?;
            let my_half_deposit_psbt = trade_model.my_half_deposit_psbt()?;
            let my_nonce_shares = trade_model.my_nonce_shares()?;

            Ok(NonceSharesMessage {
                half_deposit_psbt: my_half_deposit_psbt.serialize(),
                redirection_amount_msat,
                ..(my_addresses, my_nonce_shares).into()
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_partial_signatures(
        &self,
        request: Request<PartialSignaturesRequest>,
    ) -> Result<Response<PartialSignaturesMessage>> {
        self.handle_trade_request(request, |request, trade_model, _wallet| {
            if request.buyer_ready_to_release {
                trade_model.start_payment()?;
            } else {
                let peers_nonce_shares = request
                    .peers_nonce_shares
                    .ok_or_else(|| Status::not_found("missing request.peers_nonce_shares"))?;
                let redirection_receivers = request
                    .redirection_receivers
                    .into_iter()
                    .map(TryProtoInto::try_proto_into)
                    .collect::<Result<Vec<_>>>()?;
                let peers_message = (|| {
                    let half_deposit_psbt: Psbt =
                        (&peers_nonce_shares.half_deposit_psbt[..]).try_proto_into()?;
                    let (addresses, nonces) = peers_nonce_shares.try_proto_into()?;
                    Ok((half_deposit_psbt, addresses, nonces))
                })()
                .peer_data();
                trade_model.receive_nonce_shares(peers_message, redirection_receivers)?;
            }
            let my_partial_signatures = trade_model.my_partial_signatures_on_peer_txs()?;

            Ok(my_partial_signatures.into())
        })
        .await
    }

    #[instrument(skip_all)]
    async fn sign_deposit_tx(
        &self,
        request: Request<DepositTxSignatureRequest>,
    ) -> Result<Response<DepositPsbt>> {
        self.handle_trade_request(request, |request, trade_model, wallet| {
            let peers_partial_signatures = request
                .peers_partial_signatures
                .ok_or_else(|| Status::not_found("missing request.peers_partial_signatures"))?;
            let peers_partial_signatures: PeerData<ExchangedSigs<ByVal>> =
                peers_partial_signatures.try_proto_into().peer_data();
            trade_model.receive_partial_signatures(wallet, peers_partial_signatures)?;
            let deposit_psbt = trade_model.deposit_psbt()?;

            Ok(DepositPsbt {
                deposit_psbt: deposit_psbt.serialize(),
            })
        })
        .await
    }

    type PublishDepositTxStream = TracedResultStream<TxConfirmationStatus>;

    #[instrument(skip_all)]
    async fn publish_deposit_tx(
        &self,
        request: Request<PublishDepositTxRequest>,
    ) -> Result<Response<Self::PublishDepositTxStream>> {
        // Checked first: without a peer that the transaction can be sent to, the request changes
        // nothing.
        let chain = self.chain_for_sending()?;
        self.handle_trade_request(request, move |request, trade_model, wallet| {
            let peers_deposit_psbt = request
                .peers_deposit_psbt
                .ok_or_else(|| Status::not_found("missing request.peers_deposit_psbt"))?;
            let peers_deposit_psbt: PeerData<Psbt> =
                peers_deposit_psbt.deposit_psbt.try_proto_into().peer_data();
            let deposit_tx = trade_model.receive_deposit_psbt(wallet, peers_deposit_psbt)?;
            // The task that watches the trades sends the transaction, and sends it again until
            // it is confirmed or the deposit timeout has passed. This does not depend on the
            // stream of this response.

            Ok(tx_confirmation_status_stream(
                chain,
                request.trade_id,
                deposit_tx.compute_txid(),
                Some(consensus::serialize(&deposit_tx)),
            )
            .box_traced())
        })
        .await
    }

    type SubscribeTxConfirmationStatusStream = TracedResultStream<TxConfirmationStatus>;

    #[instrument(skip_all)]
    async fn subscribe_tx_confirmation_status(
        &self,
        request: Request<SubscribeTxConfirmationStatusRequest>,
    ) -> Result<Response<Self::SubscribeTxConfirmationStatusStream>> {
        let chain = self.chain()?;
        self.handle_trade_request(request, move |request, trade_model, _wallet| {
            // Before the trader has signed the deposit transaction there is nothing to observe.
            if trade_model.state() == TradeState::Init {
                return Err(ProtocolErrorKind::NotAllowedInState(TradeState::Init).into());
            }
            let deposit_txid = trade_model.deposit_txid()?;

            Ok(
                tx_confirmation_status_stream(chain, request.trade_id, deposit_txid, None)
                    .box_traced(),
            )
        })
        .await
    }

    #[instrument(skip_all)]
    async fn sign_swap_tx(
        &self,
        request: Request<SwapTxSignatureRequest>,
    ) -> Result<Response<SwapTxSignatureResponse>> {
        self.handle_trade_request(request, |request, trade_model, _wallet| {
            if request.seller_ready_to_release {
                let prv_key_share = trade_model.release_payout()?;

                Ok(SwapTxSignatureResponse {
                    peer_output_prv_key_share: Some(prv_key_share.to_bytes().to_vec()),
                })
            } else {
                let signature: PeerData<PartialSignature> = request
                    .swap_tx_input_peers_partial_signature
                    .try_proto_into()
                    .peer_data();
                trade_model.receive_swap_tx_signature(signature)?;

                Ok(SwapTxSignatureResponse::default())
            }
        })
        .await
    }

    #[instrument(skip_all)]
    async fn close_trade(
        &self,
        request: Request<CloseTradeRequest>,
    ) -> Result<Response<CloseTradeResponse>> {
        self.handle_trade_request(request, |request, trade_model, wallet| {
            let penalty_tx_fee_rate = request
                .penalty_tx_fee_rate
                .map(u64::check_in_signed_range)
                .transpose()?
                .map(FeeRate::from_sat_per_kwu);
            if let Some(peer_prv_key_share) = request.my_output_peers_prv_key_share {
                // Trader receives the private key share from a cooperative peer, closing our trade.
                // The bytes of the request are wiped when this block ends.
                let peer_prv_key_share = Zeroizing::new(peer_prv_key_share);
                let peer_prv_key_share: PeerData<SecretScalar> =
                    (&peer_prv_key_share[..]).try_proto_into().peer_data();
                trade_model.close_with_key_share(
                    wallet,
                    peer_prv_key_share,
                    penalty_tx_fee_rate,
                )?;
            } else if let Some(swap_tx) = request.swap_tx {
                // Buyer supplies a signed swap tx to the Rust server, to close our trade. (Mainly
                // for testing -- normally the tx would be picked up from the
                // bitcoin network by the server.)
                let swap_tx: Transaction = swap_tx.try_proto_into()?;
                trade_model.close_with_swap_tx(wallet, &swap_tx, penalty_tx_fee_rate)?;
            } else {
                // Peer unresponsive -- force-close our trade by publishing the swap tx. For seller
                // only.
                // The task that watches the trades sends the swap transaction.
                trade_model.close_by_force()?;
            }
            let my_prv_key_share = trade_model.my_private_key_share_for_peer_output().ok();
            // We could use `oneof` in the proto for these fields, instead of a (wire-compatible)
            // pair of optionals, but that makes the Prost bindings awkward to use:
            let swap_tx = trade_model.signed_swap_tx().ok();
            let penalty_tx = trade_model
                .signed_penalty_tx()
                .ok()
                .filter(|_| swap_tx.is_none());

            Ok(CloseTradeResponse {
                peer_output_prv_key_share: my_prv_key_share.map(|s| s.to_bytes().to_vec()),
                swap_tx: swap_tx.map(consensus::serialize),
                penalty_tx: penalty_tx.map(consensus::serialize),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn sign_custom_payout_tx(
        &self,
        request: Request<CustomPayoutPsbtRequest>,
    ) -> Result<Response<CustomPayoutPsbt>> {
        self.handle_trade_request(request, |request, trade_model, wallet| {
            trade_model.sign_custom_payout(
                wallet,
                Amount::from_sat(
                    request
                        .sellers_payout_amount_excluding_fee
                        .check_in_signed_range()?,
                ),
                FeeRate::from_sat_per_kwu(request.fee_rate.check_in_signed_range()?),
            )?;
            let psbt = trade_model.custom_payout_psbt()?;

            Ok(CustomPayoutPsbt {
                psbt: psbt.serialize(),
                tx_id: psbt.unsigned_tx.compute_txid().to_string(),
                buyers_payout_amount_including_fee: psbt.unsigned_tx.output[0].value.to_sat(),
                sellers_payout_amount_including_fee: psbt.unsigned_tx.output[1].value.to_sat(),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn custom_close_trade(
        &self,
        request: Request<CustomCloseTradeRequest>,
    ) -> Result<Response<CustomCloseTradeResponse>> {
        self.handle_trade_request(request, |request, trade_model, wallet| {
            let peers_psbt: PeerData<Psbt> = request
                .peers_custom_payout_psbt
                .try_proto_into()
                .peer_data();
            // The task that watches the trades sends the custom payout transaction.
            let custom_payout_tx = trade_model.finalize_custom_payout(wallet, peers_psbt)?;

            Ok(CustomCloseTradeResponse {
                custom_payout_tx: consensus::serialize(&custom_payout_tx),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn abandon_trade(
        &self,
        request: Request<AbandonTradeRequest>,
    ) -> Result<Response<AbandonTradeResponse>> {
        self.handle_trade_request(request, |_request, trade_model, wallet| {
            trade_model.abandon(wallet)?;

            Ok(AbandonTradeResponse {})
        })
        .await
    }

    #[instrument(skip_all)]
    async fn publish_warning_tx(
        &self,
        request: Request<PublishWarningTxRequest>,
    ) -> Result<Response<PublishWarningTxResponse>> {
        // Checked first: the decision to start a dispute is not taken while the transaction
        // cannot be sent.
        self.chain_for_sending()?;
        self.handle_trade_request(request, |_request, trade_model, _wallet| {
            // The task that watches the trades sends the warning transaction.
            let warning_tx = trade_model.start_warning()?;

            Ok(PublishWarningTxResponse {
                warning_tx: consensus::serialize(&warning_tx),
            })
        })
        .await
    }

    type SubscribeTradeEventsStream = TracedResultStream<TradeEvent>;

    #[instrument(skip_all)]
    async fn subscribe_trade_events(
        &self,
        request: Request<SubscribeTradeEventsRequest>,
    ) -> Result<Response<Self::SubscribeTradeEventsStream>> {
        // Subscribed before the state is read, so that no change of the state is lost.
        let events = self.events.subscribe();
        self.handle_trade_request(request, move |request, trade_model, _wallet| {
            let first = trade_event(trade_model, self.chain.as_deref(), TradeEventType::State);
            Ok(trade_event_stream(request.trade_id, first, events).box_traced())
        })
        .await
    }
}

/// The number of confirmations at which a transaction counts as confirmed.
const MIN_CONFIRMATIONS: u32 = protocol::params::MIN_CONFIRMATIONS as u32;

/// The pause before a transaction that could not be sent is sent again.
const SEND_RETRY_DELAY: Duration = Duration::from_secs(5);
/// The longest time between two looks at the trades, also without an event of the chain.
const WATCH_INTERVAL: Duration = Duration::from_secs(30);

/// Stores the record of a trade in the wallet. The record must be on disk before a signature, a
/// key share or a transaction of the trade leaves the service.
fn store_trade(trade_model: &TradeModel, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
    let stored = trade_model
        .to_record()
        .map_err(|error| error.to_string())
        .and_then(|record| {
            wallet
                .store_trade_record(trade_model.trade_id(), &record)
                .map_err(|error| error.to_string())
        });
    stored.map_err(|error| {
        // The reason lies outside of the trade, for example a full disk, and the request can
        // be repeated.
        ErrorCase::Temporary.of(Status::internal(format!(
            "the trade {} could not be stored, so nothing of this request is handed out: {error}",
            trade_model.trade_id()
        )))
    })
}

/// Watches the outputs of a trade on the chain, once per trade in a run of the program, from the
/// moment the trader has signed the deposit transaction.
///
/// The outputs are watched from the block at which the trader signed the deposit. For a trade
/// that was loaded after a restart, this makes the chain client evaluate the blocks that were
/// mined while the program did not run.
fn ensure_watched(
    chain: &dyn ChainAccess,
    watched: &Mutex<HashSet<String>>,
    trade_model: &TradeModel,
    wallet: &dyn ProtocolWalletApi,
) {
    // Before the chain is known, the chain client cannot be told where to start.
    if chain.tip_height().is_none() {
        return;
    }
    let Ok(outputs) = trade_model.outputs_to_watch() else {
        return;
    };
    if !watched
        .lock()
        .unwrap()
        .insert(trade_model.trade_id().to_owned())
    {
        return;
    }
    let start = trade_model
        .deposit_signing_height()
        .and_then(|height| wallet.known_block_at_or_below(height));
    match start {
        Some(start) => chain.watch_outputs_from(outputs, start),
        None => {
            for (outpoint, script) in outputs {
                chain.watch_output(outpoint, script);
            }
        }
    }
}

/// Loads the trades of a wallet into the trade store, when the wallet is opened. This is the
/// restart of the trades:
///
/// 1. Every record of the wallet is loaded. If one record cannot be loaded, no trade is loaded and
///    the store is marked as unusable: the trade service then refuses every trade request.
/// 2. A trade whose deposit transaction the trader has not signed is abandoned, and its coins are
///    released. Its nonces are lost, so its setup cannot continue.
/// 3. The reservations of the wallet are made to agree with the trades: a coin that no trade
///    reserves is released, and a coin that a trade reserves is reserved again.
///
/// The task that watches the trades then watches their outputs from the block of each deposit
/// signature, and performs the reactions for what it finds.
pub fn restore_trades(wallet: &mut dyn ProtocolWalletApi, trades: &dyn TradeModelStore) {
    let records = match wallet.trade_records() {
        Ok(records) => records,
        Err(error) => {
            error!(%error, "The trade records of the wallet cannot be read.");
            trades.set_unusable(error.to_string());
            return;
        }
    };
    let mut loaded = Vec::with_capacity(records.len());
    let mut failures = Vec::new();
    for (trade_id, record) in records {
        match TradeModel::from_record(&record, wallet) {
            Ok(trade_model) if trade_model.trade_id() == trade_id => loaded.push(trade_model),
            Ok(trade_model) => failures.push(format!(
                "trade {trade_id}: the record belongs to the trade {}",
                trade_model.trade_id()
            )),
            Err(error) => failures.push(format!("trade {trade_id}: {error}")),
        }
    }
    if !failures.is_empty() {
        let reason = failures.join("; ");
        error!(
            reason,
            "The trades of the wallet cannot be loaded. No trade is served."
        );
        trades.set_unusable(reason);
        return;
    }
    let mut reserved = HashSet::new();
    for mut trade_model in loaded {
        let trade_id = trade_model.trade_id().to_owned();
        match trade_model.abandon_after_restart(wallet) {
            Ok(true) => {
                info!(
                    trade_id,
                    "Abandoned a trade whose setup was interrupted by a restart."
                );
                if let Err(error) = store_trade(&trade_model, wallet) {
                    error!(trade_id, %error, "Trade not stored.");
                }
            }
            Ok(false) => {}
            Err(error) => error!(trade_id, %error, "Could not abandon an interrupted setup."),
        }
        reserved.extend(trade_model.reserved_coins().iter().copied());
        info!(trade_id, state = %trade_model.state(), "Loaded a trade.");
        if let Err(error) = trades.add_trade_model(trade_model) {
            error!(trade_id, %error, "Could not load a trade.");
        }
    }
    // A stop of the program between the reservation of a coin and the storing of its trade, or
    // the other way round, leaves the two apart.
    for coin in wallet.list_locked_utxos() {
        if !reserved.contains(&coin) {
            info!(%coin, "Releasing a reserved coin that no trade reserves.");
            if let Err(error) = wallet.unlock_utxo(coin) {
                error!(%coin, %error, "Could not release a coin.");
            }
        }
    }
    for coin in reserved {
        if wallet.is_utxo_locked(coin) {
            continue;
        }
        if let Err(error) = wallet.lock_utxo(coin) {
            error!(%coin, %error, "Could not reserve the coin of a trade again.");
        }
    }
}

/// How long a request waits for the chain client to evaluate the newest block.
const CHAIN_EVALUATION_WAIT: Duration = Duration::from_secs(3);

/// Waits until the chain client has evaluated all blocks that it knows, for a short time.
///
/// The header of a new block is known a moment before the block is evaluated. In that moment
/// the confirmation events of the deposit already count the block, but the engine cannot be told
/// the facts of the chain yet. A caller that sends a request at once after such an event would
/// meet an engine that does not know about the confirmation. A longer evaluation, as after a
/// restart, is not waited for: the request is then answered with the facts that the engine has.
async fn wait_until_evaluated(chain: &dyn ChainAccess) {
    let deadline = tokio::time::Instant::now() + CHAIN_EVALUATION_WAIT;
    while chain.tip_height().is_some()
        && chain.synced_height() != chain.tip_height()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads the facts about the chain for one trade. `None` if the chain is not known yet, or if
/// the trader has not signed the deposit transaction, so that there is nothing to observe.
fn chain_facts(chain: &dyn ChainAccess, trade_model: &TradeModel) -> Option<ChainFacts> {
    let tip_height = chain.tip_height()?;
    // While the chain client evaluates earlier blocks, for example for a trade that was loaded
    // after a restart, it does not know yet what happened in them.
    if chain.synced_height() != Some(tip_height) {
        return None;
    }
    let deposit_txid = trade_model.deposit_txid().ok()?;
    let outputs = trade_model.outputs_to_watch().ok()?;
    Some(ChainFacts::collect(
        tip_height,
        &deposit_txid,
        outputs.into_iter().map(|(outpoint, _)| outpoint),
        |txid| chain.confirmations(txid),
        |outpoint| chain.spending_tx(outpoint).map(|found| found.tx),
    ))
}

/// Tells the engine the current facts about the chain for one trade, and publishes the events
/// that follow. Returns `false` if the facts are not known at the moment, so that the engine was
/// told nothing.
fn observe_chain(
    chain: &dyn ChainAccess,
    trade_model: &mut TradeModel,
    wallet: &mut dyn ProtocolWalletApi,
    events: &broadcast::Sender<TradeEvent>,
) -> bool {
    let Some(facts) = chain_facts(chain, trade_model) else {
        return false;
    };
    for event in trade_model.observe_chain(wallet, facts) {
        let mut confirmed = None;
        let (event_type, message) = match event {
            EngineEvent::StateChanged { .. } => (TradeEventType::State, None),
            EngineEvent::Loss(loss) => (TradeEventType::Loss, Some(loss)),
            EngineEvent::Unexpected(what) => (TradeEventType::Unexpected, Some(what)),
            EngineEvent::StatusChanged => (TradeEventType::Status, None),
            EngineEvent::TxConfirmations(tx) => {
                confirmed = Some(tx);
                (TradeEventType::TxConfirmed, None)
            }
            _ => continue,
        };
        if let Some(message) = &message {
            warn!(
                trade_id = trade_model.trade_id(),
                ?event_type,
                message,
                "Trade event."
            );
        }
        // An error means that nobody is subscribed.
        let _ = events.send(TradeEvent {
            message,
            tx_kind: confirmed.map(|tx| tx_kind(tx.kind).into()),
            tx_id: confirmed.map(|tx| tx.txid.to_string()),
            num_confirmations: confirmed.map(|tx| tx.confirmations),
            ..trade_event(trade_model, Some(chain), event_type)
        });
    }
    true
}

/// An event of a trade with the current state of the trade.
fn trade_event(
    trade_model: &TradeModel,
    chain: Option<&dyn ChainAccess>,
    event_type: TradeEventType,
) -> TradeEvent {
    let (state, closure_type) = match trade_model.state() {
        TradeState::Init => (TradeStateKind::Init, None),
        TradeState::Deposit => (TradeStateKind::Deposit, None),
        TradeState::BuyerReadyToRelease => (TradeStateKind::BuyerReadyToRelease, None),
        TradeState::SellerReadyToRelease => (TradeStateKind::SellerReadyToRelease, None),
        TradeState::CustomPayoutSigned => (TradeStateKind::CustomPayoutSigned, None),
        TradeState::BuyersWarning => (TradeStateKind::BuyersWarning, None),
        TradeState::SellersWarning => (TradeStateKind::SellersWarning, None),
        TradeState::TradeClosed(closure_type) => (
            TradeStateKind::TradeClosed,
            Some(match closure_type {
                ClosureType::Cooperative => ClosureKind::Cooperative,
                ClosureType::Forced => ClosureKind::Forced,
                ClosureType::Custom => ClosureKind::Custom,
                ClosureType::BuyersRedirect => ClosureKind::BuyersRedirect,
                ClosureType::SellersRedirect => ClosureKind::SellersRedirect,
                ClosureType::BuyersClaim => ClosureKind::BuyersClaim,
                ClosureType::SellersClaim => ClosureKind::SellersClaim,
                ClosureType::BuyersPenaltyTx => ClosureKind::BuyersPenaltyTx,
                ClosureType::SellersPenaltyTx => ClosureKind::SellersPenaltyTx,
                ClosureType::Abandoned => ClosureKind::Abandoned,
                ClosureType::DepositAborted => ClosureKind::DepositAborted,
            }),
        ),
    };
    let status = trade_model.status();
    let deadlines = status.deadlines.iter().map(|deadline| TradeDeadline {
        kind: match deadline.kind {
            DeadlineKind::DepositTimeout => TradeDeadlineKind::DepositTimeout,
            DeadlineKind::WarningLockTime => TradeDeadlineKind::WarningLockTime,
            DeadlineKind::LastRelease => TradeDeadlineKind::LastRelease,
            DeadlineKind::ForcedClose => TradeDeadlineKind::ForcedClose,
            DeadlineKind::ClaimLockTime => TradeDeadlineKind::ClaimLockTime,
        }
        .into(),
        block_height: deadline.height,
    });
    let required_actions = status.user_actions.iter().map(|action| {
        match action {
            UserAction::PublishWarning => TradeUserAction::PublishWarning,
            UserAction::StartDispute => TradeUserAction::StartDispute,
            UserAction::SpendOrDrainPayout => TradeUserAction::SpendOrDrainPayout,
        }
        .into()
    });
    TradeEvent {
        trade_id: trade_model.trade_id().to_owned(),
        r#type: event_type.into(),
        current_block_height: chain.and_then(ChainAccess::tip_height).unwrap_or(0),
        state: state.into(),
        closure_type: closure_type.map(Into::into),
        r#final: status.is_final,
        tx_kind: None,
        tx_id: None,
        message: None,
        termination_reason: status.termination_reason,
        deadlines: deadlines.collect(),
        required_actions: required_actions.collect(),
        num_confirmations: None,
        fee_bump_tx_id: None,
    }
}

const fn tx_kind(kind: TxKind) -> TradeTxKind {
    match kind {
        TxKind::Deposit => TradeTxKind::DepositTx,
        TxKind::DepositAbort => TradeTxKind::DepositAbortTx,
        TxKind::Swap => TradeTxKind::SwapTx,
        TxKind::CustomPayout => TradeTxKind::CustomPayoutTx,
        TxKind::Warning => TradeTxKind::WarningTx,
        TxKind::Redirect => TradeTxKind::RedirectTx,
        TxKind::Claim => TradeTxKind::ClaimTx,
        TxKind::Penalty => TradeTxKind::PenaltyTx,
    }
}

/// Watches all trades on the chain for the lifetime of the service (D7, D8).
///
/// After every event of the chain, after every request for a trade, and at least every
/// [`WATCH_INTERVAL`], each trade that is not final is told the facts about the chain. The
/// transactions that its engine then names are sent to the network: the deposit, the reactions to
/// a warning transaction of the peer, the claim, the swap transaction, the custom payout.
///
/// A broadcast has no result (N2), so a transaction is sent again after every new block, as long
/// as the engine names it. A sending that fails, for example while no peer is reachable, is
/// tried again after [`SEND_RETRY_DELAY`].
async fn watch_trades(
    trades: Arc<dyn TradeModelStore>,
    wallet: Arc<dyn TradeWalletProvider>,
    chain: Arc<dyn ChainAccess>,
    events: broadcast::Sender<TradeEvent>,
    wake: Arc<Notify>,
    watched: Arc<Mutex<HashSet<String>>>,
) {
    let mut chain_events = chain.subscribe();
    // The height of the chain at which a transaction was sent last, with the child that raised
    // its fee. A new child is sent at once, also at the same height.
    let mut sent_at: HashMap<(Txid, Option<Txid>), Option<u32>> = HashMap::new();
    let mut reported_problem: Option<PeerProblem> = None;
    loop {
        let problem = chain.peer_problem();
        if problem != reported_problem {
            report_chain_problem(&*trades, &*chain, &events, problem);
            reported_problem = problem;
        }

        // The wallet is locked before a trade, as in the requests.
        let mut to_send: Vec<(TradeEvent, TxToSend)> = Vec::new();
        wallet
            .with_trade_wallet(&mut |wallet| {
                for trade_model in trades.trade_models() {
                    let mut trade_model = trade_model.lock().unwrap();
                    if trade_model.is_final() {
                        continue;
                    }
                    ensure_watched(&*chain, &watched, &trade_model, wallet);
                    let stored = trade_model.to_record().ok();
                    // Without the current facts nothing is sent: a transaction that the engine
                    // does not know can be in a block that is not evaluated yet, for example
                    // after a restart.
                    if !observe_chain(&*chain, &mut trade_model, wallet, &events) {
                        continue;
                    }
                    // What the engine decided at this look at the chain is on disk before a
                    // transaction that follows from it leaves the service.
                    if trade_model.to_record().ok() != stored {
                        if let Err(status) = store_trade(&trade_model, wallet) {
                            let trade_id = trade_model.trade_id();
                            error!(trade_id, %status, "Trade not stored.");
                            continue;
                        }
                    }
                    for tx in trade_model.transactions_to_send(engine_fee_rate()) {
                        let event =
                            trade_event(&trade_model, Some(&*chain), TradeEventType::TxSent);
                        to_send.push((event, tx));
                    }
                }
            })
            .await;

        let height = chain.tip_height();
        let mut failed = false;
        for (event, TxToSend { kind, tx, child }) in to_send {
            let txid = tx.compute_txid();
            let child_txid = child.as_ref().map(Transaction::compute_txid);
            let sent = (txid, child_txid);
            if sent_at.get(&sent) == Some(&height) {
                continue;
            }
            let first_sending = sent_at.keys().all(|(parent, _)| *parent != txid);
            let result = send_tx(&*chain, tx, child, first_sending).await;
            match result {
                // "Sent" does not mean that a node accepted the transaction.
                Ok(_) => {
                    info!(
                        trade_id = event.trade_id,
                        ?kind,
                        %txid,
                        ?child_txid,
                        ?height,
                        "Sent a transaction."
                    );
                    sent_at.insert(sent, height);
                    let _ = events.send(TradeEvent {
                        tx_kind: Some(tx_kind(kind).into()),
                        tx_id: Some(txid.to_string()),
                        fee_bump_tx_id: child_txid.map(|txid| txid.to_string()),
                        ..event
                    });
                }
                // The peer has the transaction already, or it has rejected it since the last
                // block. It is announced again after the next block.
                Err(ClientError::NotRequested(_)) => {
                    debug!(%txid, ?height, "No peer asked for the announced transaction.");
                    sent_at.insert(sent, height);
                }
                Err(error) => {
                    warn!(?kind, %txid, %error, "Could not send a transaction. Trying again.");
                    failed = true;
                }
            }
        }

        let pause = if failed {
            SEND_RETRY_DELAY
        } else {
            WATCH_INTERVAL
        };
        tokio::select! {
            event = chain_events.recv() => {
                if matches!(event, Err(RecvError::Closed)) {
                    return;
                }
            }
            () = wake.notified() => {}
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// Sends a transaction of a trade to the network. A transaction with a child that raises its
/// fee is sent with the child as one package.
///
/// A node takes some seconds longer for a package than for a single transaction, because it
/// asks for the child and then for the parent, each time after a delay. With `first_sending`,
/// for a transaction that this run of the program has not sent before, the transaction
/// therefore goes out alone first: a reaction with a deadline must not wait for the package. A
/// node that refuses the single transaction for its fee still accepts it in the package.
async fn send_tx(
    chain: &dyn ChainAccess,
    tx: Transaction,
    child: Option<Transaction>,
    first_sending: bool,
) -> Result<Txid, ClientError> {
    let Some(child) = child else {
        return chain.broadcast(tx).await;
    };
    if first_sending {
        if let Err(error) = chain.broadcast(tx.clone()).await {
            debug!(%error, "Transaction not sent alone before its package.");
        }
    }
    chain.broadcast_package(tx, child).await
}

/// Tells the subscribers of all trades that are not final about the start or the end of a
/// problem with the peers (D21): while it lasts, the chain is not observed.
fn report_chain_problem(
    trades: &dyn TradeModelStore,
    chain: &dyn ChainAccess,
    events: &broadcast::Sender<TradeEvent>,
    problem: Option<PeerProblem>,
) {
    let event_type = if problem.is_some() {
        TradeEventType::ChainProblem
    } else {
        TradeEventType::ChainProblemOver
    };
    for trade_model in trades.trade_models() {
        let trade_model = trade_model.lock().unwrap();
        if trade_model.is_final() {
            continue;
        }
        let _ = events.send(TradeEvent {
            message: problem.map(|problem| problem.to_string()),
            ..trade_event(&trade_model, Some(chain), event_type)
        });
    }
}

/// The events of one trade: first the event `first`, then the events of the trade as they
/// happen.
fn trade_event_stream(
    trade_id: String,
    first: TradeEvent,
    events: broadcast::Receiver<TradeEvent>,
) -> impl Stream<Item = Result<TradeEvent>> {
    let later = stream::unfold((events, trade_id), |(mut events, trade_id)| async move {
        loop {
            match events.recv().await {
                Ok(event) if event.trade_id == trade_id => {
                    return Some((Ok(event), (events, trade_id)));
                }
                // An event of another trade, or events that this subscriber lost.
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    });
    stream::once(async move { Ok(first) }).chain(later)
}

/// The events about the confirmation of a transaction: one event for every change of its number
/// of confirmations that the service observes, from the first confirmation on. Blocks that arrive
/// together can give one event, so a number can be left out. The stream ends when the transaction
/// counts as confirmed. After a reorganisation the number can fall, also to 0.
///
/// `signed_tx` is the transaction, if the caller holds it. Otherwise the events contain the
/// transaction as it was found in a block.
fn tx_confirmation_status_stream(
    chain: Arc<dyn ChainAccess>,
    trade_id: String,
    txid: Txid,
    signed_tx: Option<Vec<u8>>,
) -> impl Stream<Item = Result<TxConfirmationStatus>> {
    let events = chain.subscribe();
    let state = (chain, events, None::<u32>, signed_tx);
    stream::unfold(
        state,
        move |(chain, mut events, reported, signed_tx)| async move {
            if reported.is_some_and(|confirmations| confirmations >= MIN_CONFIRMATIONS) {
                return None;
            }
            loop {
                let confirmations = chain.confirmations(&txid);
                let changed = match reported {
                    Some(reported) => reported != confirmations,
                    None => confirmations > 0,
                };
                if changed {
                    let tx = signed_tx.clone().unwrap_or_else(|| {
                        chain
                            .transaction(&txid)
                            .map(|found| consensus::serialize(&*found.tx))
                            .unwrap_or_default()
                    });
                    let status = TxConfirmationStatus {
                        tx,
                        current_block_height: chain.tip_height().unwrap_or(0),
                        num_confirmations: confirmations,
                    };
                    return Some((Ok(status), (chain, events, Some(confirmations), signed_tx)));
                }
                match events.recv().await {
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    )
    .on_drop(move || {
        debug!(
            trade_id,
            "Deposit tx confirmation status stream has been dropped."
        );
    })
}

struct LazyJson<T>(T);

impl<T: Serialize> Display for LazyJson<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = if f.alternate() {
            serde_json::to_string_pretty(&self.0)
        } else {
            serde_json::to_string(&self.0)
        };
        write!(f, "{}", s.as_deref().unwrap_or("<<SERIALIZATION ERROR>>"))
    }
}

#[derive(Serialize)]
pub struct TracedResultStream<T> {
    #[serde(skip)]
    inner: BoxStream<'static, Result<T>>,
    #[serde(skip)]
    span: Span,
}

impl<T> Stream for TracedResultStream<T> {
    type Item = Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _enter = this.span.enter();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

pub trait TracedResultStreamExt<T: Serialize>:
    TryStream<Ok = T, Error = Status> + Sized + Send + 'static
{
    fn box_traced(self) -> TracedResultStream<T> {
        TracedResultStream {
            inner: Box::pin(self.inspect_ok(move |event| {
                let message = LazyJson(event);
                trace!(%message, "Streaming event.");
            })),
            span: Span::current(),
        }
    }
}

impl<S> TracedResultStreamExt<S::Ok> for S
where
    S: TryStream<Error = Status> + Sized + Send + 'static,
    S::Ok: Serialize,
{
}

trait MusigRequest: Serialize {
    fn trade_id(&self) -> &str;
}

macro_rules! impl_musig_req {
    ($request_type:ty) => {
        impl MusigRequest for $request_type {
            fn trade_id(&self) -> &str {
                &self.trade_id
            }
        }
    };
}

impl_musig_req!(AbandonTradeRequest);
impl_musig_req!(PartialSignaturesRequest);
impl_musig_req!(NonceSharesRequest);
impl_musig_req!(DepositTxSignatureRequest);
impl_musig_req!(PublishDepositTxRequest);
impl_musig_req!(SubscribeTxConfirmationStatusRequest);
impl_musig_req!(SwapTxSignatureRequest);
impl_musig_req!(CloseTradeRequest);
impl_musig_req!(CustomPayoutPsbtRequest);
impl_musig_req!(CustomCloseTradeRequest);
impl_musig_req!(PublishWarningTxRequest);
impl_musig_req!(SubscribeTradeEventsRequest);

/// The error of a `musigrpc.Musig` handler. The trade engine does not know gRPC, so its errors
/// are converted to a status here, when the response is sent.
enum HandlerError {
    /// A status that the handler created itself. It concerns the request of the caller, not the
    /// data of the peer.
    Status(Status),
    /// An error of the trade engine.
    Protocol(ProtocolErrorKind),
}

impl From<Status> for HandlerError {
    fn from(value: Status) -> Self {
        Self::Status(value)
    }
}

impl From<ProtocolErrorKind> for HandlerError {
    fn from(value: ProtocolErrorKind) -> Self {
        Self::Protocol(value)
    }
}

impl From<HandlerError> for Status {
    fn from(value: HandlerError) -> Self {
        // TODO: The status codes are not decided yet (open point O11 of the specification).
        match value {
            HandlerError::Status(status) => ErrorCase::of_unmarked(status),
            HandlerError::Protocol(e) => match e {
                ProtocolErrorKind::DuplicateTradeId(_) => {
                    ErrorCase::TradeIdUsed.of(Self::already_exists(e.to_string()))
                }
                ProtocolErrorKind::TradeTerminated(_) => {
                    ErrorCase::Terminated.of(Self::aborted(e.to_string()))
                }
                ProtocolErrorKind::TradeParams(_)
                | ProtocolErrorKind::InsufficientRedirectionFunds { .. }
                | ProtocolErrorKind::ExcessRedirectionFunds { .. } => {
                    ErrorCase::Refused.of(Self::invalid_argument(e.to_string()))
                }
                ProtocolErrorKind::ChainCondition(_) => {
                    ErrorCase::RefusedForNow.of(Self::failed_precondition(e.to_string()))
                }
                e if e.is_refusal() => {
                    ErrorCase::Refused.of(Self::failed_precondition(e.to_string()))
                }
                e => ErrorCase::Internal.of(Self::internal(e.to_string())),
            },
        }
    }
}

/// Marks the conversion of data that the peer created. The engine decides what a failed
/// conversion means for the trade, after it has checked the preconditions of the request.
trait PeerField<T> {
    fn peer_data(self) -> PeerData<T>;
}

impl<T> PeerField<T> for Result<T> {
    fn peer_data(self) -> PeerData<T> {
        self.map_err(|status| UndecodablePeerData(status.message().to_owned()))
    }
}

fn handle_request<Req, Res, F>(request: Request<Req>, handler: F) -> Result<Response<Res>>
where
    Req: Serialize,
    Res: Serialize,
    F: FnOnce(Req) -> Result<Res, HandlerError>,
{
    let message = LazyJson(request.get_ref());
    debug!(%message, "Got a request.");

    let response = handler(request.into_inner())
        .map_err(Status::from)
        .inspect_err(|e| error!("Error response: {e}"))?;

    let message = LazyJson(&response);
    trace!(%message, "Sending response.");
    Ok(Response::new(response))
}

/// As [`handle_request`], but for handlers that need to await — e.g. anything taking an async
/// lock on the wallet.
pub(crate) async fn handle_request_async<Req, Res, F, Fut>(
    request: Request<Req>,
    handler: F,
) -> Result<Response<Res>>
where
    Req: Serialize,
    Res: Serialize,
    F: FnOnce(Req) -> Fut,
    Fut: Future<Output = Result<Res>>,
{
    let message = LazyJson(request.get_ref());
    debug!(%message, "Got a request.");

    let response = handler(request.into_inner())
        .await
        .inspect_err(|e| error!("Error response: {e}"))?;

    let message = LazyJson(&response);
    trace!(%message, "Sending response.");
    Ok(Response::new(response))
}
