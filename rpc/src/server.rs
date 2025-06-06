use bdk_wallet::bitcoin::{Amount, FeeRate};
use futures::stream::{self, BoxStream, StreamExt as _};
use std::iter;
use std::marker::{Send, Sync};
use std::sync::Arc;
use tonic::{Request, Response, Result, Status};

use crate::pb::convert::TryProtoInto;
use crate::pb::musigrpc::{CloseTradeRequest, CloseTradeResponse, DepositPsbt,
    DepositTxSignatureRequest, NonceSharesMessage, NonceSharesRequest, PartialSignaturesMessage,
    PartialSignaturesRequest, PubKeySharesRequest, PubKeySharesResponse, PublishDepositTxRequest,
    SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest, SwapTxSignatureResponse,
    TxConfirmationStatus};
use crate::pb::musigrpc::musig_server;
use crate::pb::walletrpc::{ConfEvent, ConfRequest, ListUnspentRequest, ListUnspentResponse,
    NewAddressRequest, NewAddressResponse, WalletBalanceRequest, WalletBalanceResponse};
use crate::pb::walletrpc::wallet_server;
use crate::protocol::{TradeModel, TradeModelStore as _, TRADE_MODELS};
use crate::wallet::WalletService;

pub use musig_server::MusigServer;
pub use wallet_server::WalletServer;

#[derive(Debug, Default)]
pub struct MusigImpl {}

// FIXME: At present, the Musig service passes some fields to the Java client that should be kept
//  secret for a time before passing them to the peer, namely the buyer's partial signature on the
//  swap tx and the seller's private key share for the buyer payout. Premature revelation of those
//  secrets would allow the seller to close the trade before the buyer starts payment, or the buyer
//  to close the trade before the seller had a chance to confirm receipt of payment (but after the
//  buyer starts payment), respectively. This should probably be changed, as the Java client should
//  never hold secrets which directly control funds (but doing so makes the RPC interface a little
//  bigger and less symmetrical.)
#[tonic::async_trait]
impl musig_server::Musig for MusigImpl {
    async fn init_trade(&self, request: Request<PubKeySharesRequest>) -> Result<Response<PubKeySharesResponse>> {
        println!("Got a request: {request:?}");

        let request = request.into_inner();
        let mut trade_model = TradeModel::new(request.trade_id, request.my_role.try_proto_into()?);
        trade_model.init_my_key_shares();
        let my_key_shares = trade_model.get_my_key_shares()
            .ok_or_else(|| Status::internal("missing key shares"))?;
        let response = PubKeySharesResponse {
            buyer_output_pub_key_share: my_key_shares[0].pub_key.serialize().into(),
            seller_output_pub_key_share: my_key_shares[1].pub_key.serialize().into(),
            current_block_height: 900_000,
        };
        TRADE_MODELS.add_trade_model(trade_model);

        Ok(Response::new(response))
    }

    async fn get_nonce_shares(&self, request: Request<NonceSharesRequest>) -> Result<Response<NonceSharesMessage>> {
        handle_request(request, move |request, trade_model| {
            trade_model.set_peer_key_shares(
                request.buyer_output_peers_pub_key_share.try_proto_into()?,
                request.seller_output_peers_pub_key_share.try_proto_into()?);
            trade_model.aggregate_key_shares()?;
            trade_model.trade_amount = Some(Amount::from_sat(request.trade_amount));
            trade_model.buyers_security_deposit = Some(Amount::from_sat(request.buyers_security_deposit));
            trade_model.sellers_security_deposit = Some(Amount::from_sat(request.sellers_security_deposit));
            trade_model.deposit_tx_fee_rate = Some(FeeRate::from_sat_per_kwu(request.deposit_tx_fee_rate));
            trade_model.prepared_tx_fee_rate = Some(FeeRate::from_sat_per_kwu(request.prepared_tx_fee_rate));
            trade_model.init_my_fee_bump_addresses()?;
            trade_model.init_my_nonce_shares()?;

            let my_fee_bump_addresses = trade_model.get_my_fee_bump_addresses()
                .ok_or_else(|| Status::internal("missing fee bump addresses"))?;
            let my_nonce_shares = trade_model.get_my_nonce_shares()
                .ok_or_else(|| Status::internal("missing nonce shares"))?;

            Ok(NonceSharesMessage {
                warning_tx_fee_bump_address: my_fee_bump_addresses[0].to_string(),
                redirect_tx_fee_bump_address: my_fee_bump_addresses[1].to_string(),
                half_deposit_psbt: vec![],
                ..my_nonce_shares.into()
            })
        })
    }

    async fn get_partial_signatures(&self, request: Request<PartialSignaturesRequest>) -> Result<Response<PartialSignaturesMessage>> {
        handle_request(request, move |request, trade_model| {
            let peer_nonce_shares = request.peers_nonce_shares
                .ok_or_else(|| Status::not_found("missing request.peers_nonce_shares"))?;
            trade_model.set_peer_fee_bump_addresses([
                (&peer_nonce_shares.warning_tx_fee_bump_address).try_proto_into()?,
                (&peer_nonce_shares.redirect_tx_fee_bump_address).try_proto_into()?
            ])?;
            trade_model.set_redirection_receivers(request.receivers.into_iter().map(TryProtoInto::try_proto_into))?;
            trade_model.set_peer_nonce_shares(peer_nonce_shares.try_proto_into()?);
            trade_model.aggregate_nonce_shares()?;
            trade_model.sign_partial()?;
            let my_partial_signatures = trade_model.get_my_partial_signatures_on_peer_txs()
                .ok_or_else(|| Status::internal("missing partial signatures"))?;

            Ok(my_partial_signatures.into())
        })
    }

    async fn sign_deposit_tx(&self, request: Request<DepositTxSignatureRequest>) -> Result<Response<DepositPsbt>> {
        handle_request(request, move |request, trade_model| {
            let peers_partial_signatures = request.peers_partial_signatures
                .ok_or_else(|| Status::not_found("missing request.peers_partial_signatures"))?;
            trade_model.set_peer_partial_signatures_on_my_txs(&peers_partial_signatures.try_proto_into()?);
            trade_model.aggregate_partial_signatures()?;

            Ok(DepositPsbt { deposit_psbt: b"deposit_psbt".into() })
        })
    }

    type PublishDepositTxStream = BoxStream<'static, Result<TxConfirmationStatus>>;

    async fn publish_deposit_tx(&self, request: Request<PublishDepositTxRequest>) -> Result<Response<Self::PublishDepositTxStream>> {
        handle_request(request, move |_request, _trade_model| {
            // TODO: *** BROADCAST DEPOSIT TX ***

            let confirmation_event = TxConfirmationStatus {
                tx: b"signed_deposit_tx".into(),
                current_block_height: 900_001,
                num_confirmations: 1,
            };

            Ok(stream::iter(iter::once(Ok(confirmation_event))).boxed())
        })
    }

    type SubscribeTxConfirmationStatusStream = BoxStream<'static, Result<TxConfirmationStatus>>;

    async fn subscribe_tx_confirmation_status(&self, request: Request<SubscribeTxConfirmationStatusRequest>)
                                              -> Result<Response<Self::SubscribeTxConfirmationStatusStream>> {
        handle_request(request, move |_request, _trade_model| {
            let confirmation_event = TxConfirmationStatus {
                tx: b"signed_deposit_tx".into(),
                current_block_height: 900_001,
                num_confirmations: 1,
            };

            Ok(stream::iter(iter::once(Ok(confirmation_event))).boxed())
        })
    }

    async fn sign_swap_tx(&self, request: Request<SwapTxSignatureRequest>) -> Result<Response<SwapTxSignatureResponse>> {
        handle_request(request, move |request, trade_model| {
            trade_model.set_swap_tx_input_peers_partial_signature(request.swap_tx_input_peers_partial_signature.try_proto_into()?);
            trade_model.aggregate_swap_tx_partial_signatures()?;
            let sig = trade_model.compute_swap_tx_input_signature()?;
            let prv_key_share = trade_model.get_my_private_key_share_for_peer_output()
                .ok_or_else(|| Status::internal("missing private key share"))?;

            Ok(SwapTxSignatureResponse {
                // For now, just set 'swap_tx' to be the (final) swap tx signature, rather than the actual signed tx:
                swap_tx: sig.serialize().into(),
                peer_output_prv_key_share: prv_key_share.serialize().into(),
            })
        })
    }

    async fn close_trade(&self, request: Request<CloseTradeRequest>) -> Result<Response<CloseTradeResponse>> {
        handle_request(request, move |request, trade_model| {
            if let Some(peer_prv_key_share) = request.my_output_peers_prv_key_share.try_proto_into()? {
                // Trader receives the private key share from a cooperative peer, closing our trade.
                trade_model.set_peer_private_key_share_for_my_output(peer_prv_key_share)?;
                trade_model.aggregate_private_keys_for_my_output()?;
            } else if let Some(swap_tx_input_signature) = request.swap_tx.try_proto_into()? {
                // Buyer supplies a signed swap tx to the Rust server, to close our trade. (Mainly for
                // testing -- normally the tx would be picked up from the bitcoin network by the server.)
                trade_model.recover_seller_private_key_share_for_buyer_output(&swap_tx_input_signature)?;
                trade_model.aggregate_private_keys_for_my_output()?;
            } else {
                // Peer unresponsive -- force-close our trade by publishing the swap tx. For seller only.
                // TODO: *** BROADCAST SWAP TX ***
            }
            let my_prv_key_share = trade_model.get_my_private_key_share_for_peer_output()
                .ok_or_else(|| Status::internal("missing private key share"))?;

            Ok(CloseTradeResponse { peer_output_prv_key_share: my_prv_key_share.serialize().into() })
        })
    }
}

pub struct WalletImpl {
    pub wallet_service: Arc<dyn WalletService + Send + Sync>,
}

#[tonic::async_trait]
impl wallet_server::Wallet for WalletImpl {
    async fn wallet_balance(&self, request: Request<WalletBalanceRequest>) -> Result<Response<WalletBalanceResponse>> {
        println!("Got a request: {request:?}");

        let balance = self.wallet_service.balance().into();

        Ok(Response::new(balance))
    }

    async fn new_address(&self, request: Request<NewAddressRequest>) -> Result<Response<NewAddressResponse>> {
        println!("Got a request: {request:?}");

        let address = self.wallet_service.reveal_next_address();

        Ok(Response::new(NewAddressResponse {
            address: address.address.to_string(),
            derivation_path: format!("m/86'/1'/0'/0/{}", address.index),
        }))
    }

    async fn list_unspent(&self, request: Request<ListUnspentRequest>) -> Result<Response<ListUnspentResponse>> {
        println!("Got a request: {request:?}");

        let utxos: Vec<_> = self.wallet_service.list_unspent().into_iter()
            .map(Into::into)
            .collect();

        Ok(Response::new(ListUnspentResponse { utxos }))
    }

    type RegisterConfidenceNtfnStream = BoxStream<'static, Result<ConfEvent>>;

    async fn register_confidence_ntfn(&self, request: Request<ConfRequest>) -> Result<Response<Self::RegisterConfidenceNtfnStream>> {
        println!("Got a request: {request:?}");

        let txid = request.into_inner().tx_id.try_proto_into()?;
        let conf_events = self.wallet_service.get_tx_confidence_stream(txid)
            .map(|o| Ok(o.map(Into::into).unwrap_or_default()))
            .boxed();

        Ok(Response::new(conf_events))
    }
}

trait MusigRequest: std::fmt::Debug {
    fn trade_id(&self) -> &str;
}

macro_rules! impl_musig_req {
    ($request_type:ty) => {
        impl MusigRequest for $request_type {
            fn trade_id(&self) -> &str { &self.trade_id }
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

fn handle_request<Req, Res, F>(request: Request<Req>, handler: F) -> Result<Response<Res>>
    where Req: MusigRequest,
          F: FnOnce(Req, &mut TradeModel) -> Result<Res> {
    println!("Got a request: {request:?}");

    let request = request.into_inner();
    let trade_model = TRADE_MODELS.get_trade_model(request.trade_id())
        .ok_or_else(|| Status::not_found(format!("missing trade with id: {}", request.trade_id())))?;
    let response = handler(request, &mut trade_model.lock().unwrap())?;

    Ok(Response::new(response))
}
