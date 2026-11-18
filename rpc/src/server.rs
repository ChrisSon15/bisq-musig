use std::fmt::{Display, Formatter};
use std::future::Future;
use std::marker::Send;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bdk_wallet::bitcoin::{Amount, FeeRate, Psbt, Transaction, consensus};
use bdk_wallet::serde_json;
use drop_stream::DropStreamExt as _;
use futures_util::stream::{self, BoxStream, Stream, TryStream, TryStreamExt as _};
use musig2::PartialSignature;
use musig2::secp::Scalar;
use protocol::params::{EngineConfig, TradeParams};
use protocol::storage::ByVal;
use protocol::trade_model::{
    ExchangedKeys, ExchangedSigs, PeerData, ProtocolErrorKind, TradeModel, TradeModelStore,
    UndecodablePeerData,
};
use serde::Serialize;
use tokio::time::{self, Duration};
use tonic::{Request, Response, Result, Status};
use tracing::{Span, debug, error, info, instrument, trace};
use wallet::protocol_wallet_api::ProtocolWalletApi;

use crate::pb::convert::{CheckInSignedRange as _, TryProtoInto};
pub use crate::pb::musigrpc::musig_server::MusigServer;
use crate::pb::musigrpc::{
    CloseTradeRequest, CloseTradeResponse, CustomCloseTradeRequest, CustomCloseTradeResponse,
    CustomPayoutPsbt, CustomPayoutPsbtRequest, DepositPsbt, DepositTxSignatureRequest,
    NonceSharesMessage, NonceSharesRequest, PartialSignaturesMessage, PartialSignaturesRequest,
    PubKeySharesRequest, PubKeySharesResponse, PublishDepositTxRequest,
    SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest, SwapTxSignatureResponse,
    TxConfirmationStatus, musig_server,
};

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
}

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
            Err(Status::failed_precondition(
                "no wallet is open; the trade service needs the wallet",
            ))
        })
    }

    /// Looks up the trade of the request and runs `handler` with it.
    fn handle_trade_request<Req, Res, F>(
        &self,
        request: Request<Req>,
        handler: F,
    ) -> Result<Response<Res>>
    where
        Req: MusigRequest,
        Res: Serialize,
        F: FnOnce(Req, &mut TradeModel) -> Result<Res, HandlerError>,
    {
        handle_request(request, move |request| {
            let trade_model = self
                .trades
                .get_trade_model(request.trade_id())
                .ok_or_else(|| {
                    Status::not_found(format!("missing trade with id: {}", request.trade_id()))
                })?;
            let mut trade_model = trade_model.lock().unwrap();
            handler(request, &mut trade_model)
        })
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
                    current_block_height: 900_000,
                };
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
        self.with_wallet(|wallet| {
            self.handle_trade_request(request, |request, trade_model| {
                let peers_key_shares = (|| {
                    Ok(ExchangedKeys {
                        buyer_payout: request.buyer_output_peers_pub_key_share.try_proto_into()?,
                        seller_payout: request
                            .seller_output_peers_pub_key_share
                            .try_proto_into()?,
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
                    warning_lock_time: lock_time_in_blocks(request.warning_lock_time)?,
                    claim_lock_time: lock_time_in_blocks(request.claim_lock_time)?,
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
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_partial_signatures(
        &self,
        request: Request<PartialSignaturesRequest>,
    ) -> Result<Response<PartialSignaturesMessage>> {
        self.handle_trade_request(request, |request, trade_model| {
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
    }

    #[instrument(skip_all)]
    async fn sign_deposit_tx(
        &self,
        request: Request<DepositTxSignatureRequest>,
    ) -> Result<Response<DepositPsbt>> {
        self.with_wallet(|wallet| {
            self.handle_trade_request(request, |request, trade_model| {
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
        })
        .await
    }

    type PublishDepositTxStream = TracedResultStream<TxConfirmationStatus>;

    #[instrument(skip_all)]
    async fn publish_deposit_tx(
        &self,
        request: Request<PublishDepositTxRequest>,
    ) -> Result<Response<Self::PublishDepositTxStream>> {
        self.handle_trade_request(request, |request, trade_model| {
            let peers_deposit_psbt = request
                .peers_deposit_psbt
                .ok_or_else(|| Status::not_found("missing request.peers_deposit_psbt"))?;
            let peers_deposit_psbt: PeerData<Psbt> =
                peers_deposit_psbt.deposit_psbt.try_proto_into().peer_data();
            let deposit_tx = trade_model.receive_deposit_psbt(peers_deposit_psbt)?;

            info!("*** BROADCAST DEPOSIT TX ***"); // TODO: Implement broadcast.

            Ok(mock_tx_confirmation_status_stream(
                request.trade_id,
                consensus::serialize(&deposit_tx),
            )
            .box_traced())
        })
    }

    type SubscribeTxConfirmationStatusStream = TracedResultStream<TxConfirmationStatus>;

    #[instrument(skip_all)]
    async fn subscribe_tx_confirmation_status(
        &self,
        request: Request<SubscribeTxConfirmationStatusRequest>,
    ) -> Result<Response<Self::SubscribeTxConfirmationStatusStream>> {
        self.handle_trade_request(request, |request, _trade_model| {
            Ok(
                mock_tx_confirmation_status_stream(request.trade_id, b"signed_deposit_tx".into())
                    .box_traced(),
            )
        })
    }

    #[instrument(skip_all)]
    async fn sign_swap_tx(
        &self,
        request: Request<SwapTxSignatureRequest>,
    ) -> Result<Response<SwapTxSignatureResponse>> {
        self.handle_trade_request(request, |request, trade_model| {
            if request.seller_ready_to_release {
                let prv_key_share = trade_model.release_payout()?;

                Ok(SwapTxSignatureResponse {
                    peer_output_prv_key_share: Some(prv_key_share.serialize().into()),
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
    }

    #[instrument(skip_all)]
    async fn close_trade(
        &self,
        request: Request<CloseTradeRequest>,
    ) -> Result<Response<CloseTradeResponse>> {
        self.handle_trade_request(request, |request, trade_model| {
            let penalty_tx_fee_rate = request
                .penalty_tx_fee_rate
                .map(u64::check_in_signed_range)
                .transpose()?
                .map(FeeRate::from_sat_per_kwu);
            if let Some(peer_prv_key_share) = request.my_output_peers_prv_key_share {
                // Trader receives the private key share from a cooperative peer, closing our trade.
                let peer_prv_key_share: PeerData<Scalar> =
                    peer_prv_key_share.try_proto_into().peer_data();
                trade_model.close_with_key_share(peer_prv_key_share, penalty_tx_fee_rate)?;
            } else if let Some(swap_tx) = request.swap_tx {
                // Buyer supplies a signed swap tx to the Rust server, to close our trade. (Mainly
                // for testing -- normally the tx would be picked up from the
                // bitcoin network by the server.)
                let swap_tx: Transaction = swap_tx.try_proto_into()?;
                trade_model.close_with_swap_tx(&swap_tx, penalty_tx_fee_rate)?;
            } else {
                // Peer unresponsive -- force-close our trade by publishing the swap tx. For seller
                // only.
                trade_model.close_by_force()?;

                info!("*** BROADCAST SWAP TX ***"); // TODO: Implement broadcast.
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
                peer_output_prv_key_share: my_prv_key_share.map(|s| s.serialize().into()),
                swap_tx: swap_tx.map(consensus::serialize),
                penalty_tx: penalty_tx.map(consensus::serialize),
            })
        })
    }

    #[instrument(skip_all)]
    async fn sign_custom_payout_tx(
        &self,
        request: Request<CustomPayoutPsbtRequest>,
    ) -> Result<Response<CustomPayoutPsbt>> {
        self.with_wallet(|wallet| {
            self.handle_trade_request(request, |request, trade_model| {
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
        })
        .await
    }

    #[instrument(skip_all)]
    async fn custom_close_trade(
        &self,
        request: Request<CustomCloseTradeRequest>,
    ) -> Result<Response<CustomCloseTradeResponse>> {
        self.with_wallet(|wallet| {
            self.handle_trade_request(request, |request, trade_model| {
                let peers_psbt: PeerData<Psbt> = request
                    .peers_custom_payout_psbt
                    .try_proto_into()
                    .peer_data();
                let custom_payout_tx = trade_model.finalize_custom_payout(wallet, peers_psbt)?;

                info!("*** BROADCAST CUSTOM PAYOUT TX ***"); // TODO: Implement broadcast.

                Ok(CustomCloseTradeResponse {
                    custom_payout_tx: consensus::serialize(&custom_payout_tx),
                })
            })
        })
        .await
    }
}

/// Converts a lock time of a request. A relative lock time in blocks has 16 bits.
fn lock_time_in_blocks(lock_time: u32) -> Result<u16> {
    u16::try_from(lock_time)
        .map_err(|_| Status::invalid_argument(format!("lock time out of range: {lock_time}")))
}

fn mock_tx_confirmation_status_stream(
    trade_id: String,
    tx: Vec<u8>,
) -> impl Stream<Item = Result<TxConfirmationStatus>> {
    let confirmation_event = TxConfirmationStatus {
        tx,
        current_block_height: 900_001,
        num_confirmations: 1,
    };
    stream::once(async {
        time::sleep(Duration::from_secs(5)).await;
        Ok(confirmation_event)
    })
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

impl_musig_req!(PartialSignaturesRequest);
impl_musig_req!(NonceSharesRequest);
impl_musig_req!(DepositTxSignatureRequest);
impl_musig_req!(PublishDepositTxRequest);
impl_musig_req!(SubscribeTxConfirmationStatusRequest);
impl_musig_req!(SwapTxSignatureRequest);
impl_musig_req!(CloseTradeRequest);
impl_musig_req!(CustomPayoutPsbtRequest);
impl_musig_req!(CustomCloseTradeRequest);

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
            HandlerError::Status(status) => status,
            HandlerError::Protocol(e) => match e {
                ProtocolErrorKind::DuplicateTradeId(_) => Self::already_exists(e.to_string()),
                ProtocolErrorKind::TradeTerminated(_) => Self::aborted(e.to_string()),
                ProtocolErrorKind::TradeParams(_)
                | ProtocolErrorKind::InsufficientRedirectionFunds { .. }
                | ProtocolErrorKind::ExcessRedirectionFunds { .. } => {
                    Self::invalid_argument(e.to_string())
                }
                e if e.is_refusal() => Self::failed_precondition(e.to_string()),
                e => Self::internal(e.to_string()),
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
