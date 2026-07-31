package bisq;

import com.google.common.base.Preconditions;
import com.google.common.collect.ImmutableMap;
import io.grpc.Channel;
import io.grpc.Grpc;
import io.grpc.InsecureChannelCredentials;
import musigrpc.MusigGrpc;
import musigrpc.Rpc.*;

import java.util.List;
import java.util.stream.Collectors;

public class TradeProtocolClient {
    private final MusigGrpc.MusigBlockingStub stub;

    public TradeProtocolClient(Channel channel) {
        this.stub = MusigGrpc.newBlockingStub(channel);
    }

    public static void main(String[] args) {
        var channel = Grpc.newChannelBuilderForAddress(
                "127.0.0.1",
                50051,
                InsecureChannelCredentials.create()
        ).build();

        try {
            var client = new TradeProtocolClient(channel);
            client.testMusigService_twoParties(0, TradeType.TAKER_IS_BUYER, ClosureType.COOPERATIVE);
            client.testMusigService_twoParties(1, TradeType.TAKER_IS_BUYER, ClosureType.FORCED);
            client.testMusigService_twoParties(2, TradeType.TAKER_IS_SELLER, ClosureType.COOPERATIVE);
            client.testMusigService_twoParties(3, TradeType.TAKER_IS_SELLER, ClosureType.FORCED);
            client.testMusigService_twoParties(4, TradeType.TAKER_IS_BUYER, ClosureType.MEDIATED);
            client.testMusigService_twoParties(5, TradeType.TAKER_IS_BUYER, ClosureType.FORCED_AFTER_MEDIATION);
        } finally {
            channel.shutdown();
        }
    }

    private enum TradeType {
        TAKER_IS_BUYER, TAKER_IS_SELLER
    }

    private enum ClosureType {
        COOPERATIVE, FORCED, MEDIATED, FORCED_AFTER_MEDIATION
    }

    private void testMusigService_twoParties(int tradeNum, TradeType tradeType, ClosureType closureType) {
        // Two peers, buyer & seller (one taker, one maker), talk to their respective Rust servers
        // via gRPC, simulated here as two sessions (trade IDs) with the same test server.
        //
        // Communication with the gRPC server is interspersed with messages exchanged between the
        // peers. These are the messages A-G defined in $SRC_ROOT/musig_trade_protocol_messages.txt,
        // with messages A-D used to set up the trade. The Java client is (for the most part) just
        // forwarding on fields that were received in the last one or two gRPC responses.

        String buyerTradeId = "buyer-trade-" + tradeNum;
        String sellerTradeId = "seller-trade-" + tradeNum;

        switch (tradeType) {
            case TAKER_IS_BUYER -> setupTakerIsBuyerTrade(buyerTradeId, sellerTradeId);
            case TAKER_IS_SELLER -> setupTakerIsSellerTrade(buyerTradeId, sellerTradeId);
        }

        doRestOfTrade(buyerTradeId, sellerTradeId, closureType);
    }

    private void setupTakerIsBuyerTrade(String buyerTradeId, String sellerTradeId) {
        var buyerPubKeyShareResponse = stub.initTrade(PubKeySharesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setMyRole(Role.BUYER_AS_TAKER)
                .build());
        System.out.println("Got reply: " + buyerPubKeyShareResponse);

        // Buyer sends Message A to seller.

        var sellerPubKeyShareResponse = stub.initTrade(PubKeySharesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setMyRole(Role.SELLER_AS_MAKER)
                .build());
        System.out.println("Got reply: " + sellerPubKeyShareResponse);

        var sellerNonceShareMessage = stub.getNonceShares(NonceSharesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setBuyerOutputPeersPubKeyShare(buyerPubKeyShareResponse.getBuyerOutputPubKeyShare())
                .setSellerOutputPeersPubKeyShare(buyerPubKeyShareResponse.getSellerOutputPubKeyShare())
                .setPeersMultisigScriptKey(buyerPubKeyShareResponse.getMultisigScriptKey())
                .setDepositTxFeeRate(3_125)  // 12.5 sats per vbyte
                .setPreparedTxFeeRate(2_500) // 10.0 sats per vbyte
                .setTradeAmount(200_000)
                .setBuyersSecurityDeposit(30_000)
                .setSellersSecurityDeposit(30_000)
                .setTradeFeeReceiver(mockTradeFeeReceiver())
                .build());
        System.out.println("Got reply: " + sellerNonceShareMessage);

        // Seller sends Message B to buyer.

        var buyerNonceShareMessage = stub.getNonceShares(NonceSharesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setBuyerOutputPeersPubKeyShare(sellerPubKeyShareResponse.getBuyerOutputPubKeyShare())
                .setSellerOutputPeersPubKeyShare(sellerPubKeyShareResponse.getSellerOutputPubKeyShare())
                .setPeersMultisigScriptKey(sellerPubKeyShareResponse.getMultisigScriptKey())
                .setDepositTxFeeRate(3_125)  // 12.5 sats per vbyte
                .setPreparedTxFeeRate(2_500) // 10.0 sats per vbyte
                .setTradeAmount(200_000)
                .setBuyersSecurityDeposit(30_000)
                .setSellersSecurityDeposit(30_000)
                .setTradeFeeReceiver(mockTradeFeeReceiver())
                .build());
        System.out.println("Got reply: " + buyerNonceShareMessage);

        var buyerPartialSignatureMessage = stub.getPartialSignatures(PartialSignaturesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setPeersNonceShares(sellerNonceShareMessage)
                .addAllRedirectionReceivers(mockRedirectionReceivers(buyerNonceShareMessage.getRedirectionAmountMsat()))
                .build());
        System.out.println("Got reply: " + buyerPartialSignatureMessage);

        // Buyer sends Message C to seller. (Buyer's swapTxInputPartialSignature is withheld from it.)

        var sellerPartialSignatureMessage = stub.getPartialSignatures(PartialSignaturesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setPeersNonceShares(buyerNonceShareMessage)
                .addAllRedirectionReceivers(mockRedirectionReceivers(sellerNonceShareMessage.getRedirectionAmountMsat()))
                .build());
        System.out.println("Got reply: " + sellerPartialSignatureMessage);

        var sellerDepositPsbt = stub.signDepositTx(DepositTxSignatureRequest.newBuilder()
                .setTradeId(sellerTradeId)
                // Don't include contract-forming (Deposit/Warning/Redirect) txids, as they are ignored:
                .setPeersPartialSignatures(buyerPartialSignatureMessage.toBuilder().clearContractualTxIds())
                .build());
        System.out.println("Got reply: " + sellerDepositPsbt);

        // Seller subscribes to be notified of Deposit Tx confirmation:
        var sellerDepositTxConfirmationIter = stub.subscribeTxConfirmationStatus(SubscribeTxConfirmationStatusRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .build());

        // Seller sends Message D to buyer. (Seller's swapTxInputPartialSignature is NOT withheld from it.)

        var buyerDepositPsbt = stub.signDepositTx(DepositTxSignatureRequest.newBuilder()
                .setTradeId(buyerTradeId)
                // Don't include contract-forming (Deposit/Warning/Redirect) txids, as they are ignored:
                .setPeersPartialSignatures(sellerPartialSignatureMessage.toBuilder().clearContractualTxIds())
                .build());
        System.out.println("Got reply: " + buyerDepositPsbt);

        // *** BUYER BROADCASTS DEPOSIT TX ***
        var buyerDepositTxConfirmationIter = stub.publishDepositTx(PublishDepositTxRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setPeersDepositPsbt(sellerDepositPsbt)
                .build());
        // ***********************************

        // DELAY: Both traders await Deposit Tx confirmation:
        System.out.println("Awaiting Deposit Tx confirmation...\n");
        buyerDepositTxConfirmationIter.forEachRemaining(reply -> System.out.println("Got reply: " + reply));
        sellerDepositTxConfirmationIter.forEachRemaining(reply -> System.out.println("Got reply: " + reply));
    }

    private void setupTakerIsSellerTrade(String buyerTradeId, String sellerTradeId) {
        var sellerPubKeyShareResponse = stub.initTrade(PubKeySharesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setMyRole(Role.SELLER_AS_TAKER)
                .build());
        System.out.println("Got reply: " + sellerPubKeyShareResponse);

        // Seller sends Message A to buyer.

        var buyerPubKeyShareResponse = stub.initTrade(PubKeySharesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setMyRole(Role.BUYER_AS_MAKER)
                .build());
        System.out.println("Got reply: " + buyerPubKeyShareResponse);

        var buyerNonceShareMessage = stub.getNonceShares(NonceSharesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setBuyerOutputPeersPubKeyShare(sellerPubKeyShareResponse.getBuyerOutputPubKeyShare())
                .setSellerOutputPeersPubKeyShare(sellerPubKeyShareResponse.getSellerOutputPubKeyShare())
                .setPeersMultisigScriptKey(sellerPubKeyShareResponse.getMultisigScriptKey())
                .setDepositTxFeeRate(3_125)  // 12.5 sats per vbyte
                .setPreparedTxFeeRate(2_500) // 10.0 sats per vbyte
                .setTradeAmount(200_000)
                .setBuyersSecurityDeposit(30_000)
                .setSellersSecurityDeposit(30_000)
                .setTradeFeeReceiver(mockTradeFeeReceiver())
                .build());
        System.out.println("Got reply: " + buyerNonceShareMessage);

        // Buyer sends Message B to seller.

        var sellerNonceShareMessage = stub.getNonceShares(NonceSharesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setBuyerOutputPeersPubKeyShare(buyerPubKeyShareResponse.getBuyerOutputPubKeyShare())
                .setSellerOutputPeersPubKeyShare(buyerPubKeyShareResponse.getSellerOutputPubKeyShare())
                .setPeersMultisigScriptKey(buyerPubKeyShareResponse.getMultisigScriptKey())
                .setDepositTxFeeRate(3_125)  // 12.5 sats per vbyte
                .setPreparedTxFeeRate(2_500) // 10.0 sats per vbyte
                .setTradeAmount(200_000)
                .setBuyersSecurityDeposit(30_000)
                .setSellersSecurityDeposit(30_000)
                .setTradeFeeReceiver(mockTradeFeeReceiver())
                .build());
        System.out.println("Got reply: " + sellerNonceShareMessage);

        var sellerPartialSignatureMessage = stub.getPartialSignatures(PartialSignaturesRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setPeersNonceShares(buyerNonceShareMessage)
                .addAllRedirectionReceivers(mockRedirectionReceivers(sellerNonceShareMessage.getRedirectionAmountMsat()))
                .build());
        System.out.println("Got reply: " + sellerPartialSignatureMessage);

        // Seller sends Message C to buyer. (Seller's swapTxInputPartialSignature is NOT withheld from it.)

        var buyerPartialSignatureMessage = stub.getPartialSignatures(PartialSignaturesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setPeersNonceShares(sellerNonceShareMessage)
                .addAllRedirectionReceivers(mockRedirectionReceivers(buyerNonceShareMessage.getRedirectionAmountMsat()))
                .build());
        System.out.println("Got reply: " + buyerPartialSignatureMessage);

        var buyerDepositPsbt = stub.signDepositTx(DepositTxSignatureRequest.newBuilder()
                .setTradeId(buyerTradeId)
                // Don't include contract-forming (Deposit/Warning/Redirect) txids, as they are ignored:
                .setPeersPartialSignatures(sellerPartialSignatureMessage.toBuilder().clearContractualTxIds())
                .build());
        System.out.println("Got reply: " + buyerDepositPsbt);

        // Buyer subscribes to be notified of Deposit Tx confirmation:
        var buyerDepositTxConfirmationIter = stub.subscribeTxConfirmationStatus(SubscribeTxConfirmationStatusRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .build());

        // Buyer sends Message D to seller. (Buyer's swapTxInputPartialSignature is withheld from it.)

        var sellerDepositPsbt = stub.signDepositTx(DepositTxSignatureRequest.newBuilder()
                .setTradeId(sellerTradeId)
                // Don't include contract-forming (Deposit/Warning/Redirect) txids, as they are ignored:
                .setPeersPartialSignatures(buyerPartialSignatureMessage.toBuilder().clearContractualTxIds())
                .build());
        System.out.println("Got reply: " + sellerDepositPsbt);

        // *** SELLER BROADCASTS DEPOSIT TX ***
        var sellerDepositTxConfirmationIter = stub.publishDepositTx(PublishDepositTxRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setPeersDepositPsbt(buyerDepositPsbt)
                .build());
        // ***********************************

        // DELAY: Both traders await Deposit Tx confirmation:
        System.out.println("Awaiting Deposit Tx confirmation...\n");
        buyerDepositTxConfirmationIter.forEachRemaining(reply -> System.out.println("Got reply: " + reply));
        sellerDepositTxConfirmationIter.forEachRemaining(reply -> System.out.println("Got reply: " + reply));
    }

    private void doRestOfTrade(String buyerTradeId, String sellerTradeId, ClosureType closureType) {
        if (closureType == ClosureType.MEDIATED) {
            doMediatedTradeClosure(buyerTradeId, sellerTradeId);
            return;
        }

        // DELAY: Buyer makes fiat payment.

        // (Buyer calls 'GetPartialSignatures' a second time, this time with the ready-to-release flag set, so that his
        // previously withheld swapTxInputPartialSignature is revealed, to pass to the seller. All other request proto
        // fields besides the trade ID may be left as their default values.)
        var buyerPartialSignatureMessage = stub.getPartialSignatures(PartialSignaturesRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setBuyerReadyToRelease(true)
                .build());
        System.out.println("Got reply: " + buyerPartialSignatureMessage);

        // Buyer sends Message E to seller. (Includes previously withheld buyer's swapTxInputPartialSignature.)

        // (Seller should compute Swap Tx signature immediately upon receipt of Message E, instead of waiting until the
        // end of the trade, to make sure that there's no problem with it and raise a dispute ASAP otherwise.)
        var blankSwapTxSignatureResponse = stub.signSwapTx(SwapTxSignatureRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setSwapTxInputPeersPartialSignature(buyerPartialSignatureMessage.getSwapTxInputPartialSignature())
                .build());
        System.out.println("Got reply: " + blankSwapTxSignatureResponse);

        // DELAY: Seller checks buyer's fiat payment.

        // (Seller calls 'SignSwapTx' a second time, this time with the ready-to-release flag set, so that a non-blank
        // response is returned, containing the peerOutputPrvKeyShare to pass to the buyer, which releases his payout.
        // All other request proto fields besides the trade ID may be left as their default values.)
        var swapTxSignatureResponse = stub.signSwapTx(SwapTxSignatureRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setSellerReadyToRelease(true)
                .build());
        System.out.println("Got reply: " + swapTxSignatureResponse);

        if (closureType == ClosureType.COOPERATIVE) {
            // Seller sends Message F to buyer.

            // *** BUYER CLOSES TRADE ***
            var buyersCloseTradeResponse = stub.closeTrade(CloseTradeRequest.newBuilder()
                    .setTradeId(buyerTradeId)
                    .setMyOutputPeersPrvKeyShare(swapTxSignatureResponse.getPeerOutputPrvKeyShare())
                    .build());
            System.out.println("Got reply: " + buyersCloseTradeResponse);
            // **************************

            // Buyer sends Message G to seller.

            // *** SELLER CLOSES TRADE ***
            var sellersCloseTradeResponse = stub.closeTrade(CloseTradeRequest.newBuilder()
                    .setTradeId(sellerTradeId)
                    .setMyOutputPeersPrvKeyShare(buyersCloseTradeResponse.getPeerOutputPrvKeyShare())
                    .build());
            System.out.println("Got reply: " + sellersCloseTradeResponse);
            // ***************************
        } else if (closureType == ClosureType.FORCED | closureType == ClosureType.FORCED_AFTER_MEDIATION) {
            // Seller attempts to send Message F to buyer, then waits...

            if (closureType == ClosureType.FORCED_AFTER_MEDIATION) {
                // Meanwhile, buyer accepts solution from mediator (fiat received, 50% seller security deposit penalty).

                var buyersCustomPayoutPsbt = stub.signCustomPayoutTx(CustomPayoutPsbtRequest.newBuilder()
                        .setTradeId(buyerTradeId)
                        .setSellersPayoutAmountExcludingFee(15_000)
                        .setFeeRate(3_750) // 15.0 sats per vbyte
                        .build());
                System.out.println("Got reply: " + buyersCustomPayoutPsbt);

                // Buyer sends Custom Payout PSBT to seller, who cannot use it since he is already committed to a
                // non-mediated closure, through his second call to 'SignSwapTx' with the ready-to-release flag set.
            }

            // Seller never gets expected Message G from buyer -- gives up waiting.

            // *** SELLER FORCE-CLOSES TRADE ***
            var sellersCloseTradeResponse = stub.closeTrade(CloseTradeRequest.newBuilder()
                    .setTradeId(sellerTradeId)
                    .build());
            System.out.println("Got reply: " + sellersCloseTradeResponse);
            // *********************************

            // Buyer never got Message F from seller -- picks up Swap Tx from bitcoin network instead.

            // *** BUYER CLOSES TRADE ***
            var buyersCloseTradeResponse = stub.closeTrade(CloseTradeRequest.newBuilder()
                    .setTradeId(buyerTradeId)
                    .setSwapTx(swapTxSignatureResponse.getSwapTx())
                    .build());
            System.out.println("Got reply: " + buyersCloseTradeResponse);
            // **************************
        }
    }

    private void doMediatedTradeClosure(String buyerTradeId, String sellerTradeId) {
        // DELAY: Buyer accepts solution from mediator (cancelled trade, 50% buyer security deposit penalty).

        var buyersCustomPayoutPsbt = stub.signCustomPayoutTx(CustomPayoutPsbtRequest.newBuilder()
                .setTradeId(buyerTradeId)
                .setSellersPayoutAmountExcludingFee(245_000)
                .setFeeRate(3_750) // 15.0 sats per vbyte
                .build());
        System.out.println("Got reply: " + buyersCustomPayoutPsbt);

        // Buyer sends Custom Payout PSBT to seller.

        // DELAY: Seller accepts solution from mediator.

        var sellersCustomPayoutPsbt = stub.signCustomPayoutTx(CustomPayoutPsbtRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setSellersPayoutAmountExcludingFee(245_000)
                .setFeeRate(3_750) // 15.0 sats per vbyte
                .build());
        System.out.println("Got reply: " + sellersCustomPayoutPsbt);

        // *** SELLER CUSTOM-CLOSES TRADE ***
        var sellersCustomCloseTradeResponse = stub.customCloseTrade(CustomCloseTradeRequest.newBuilder()
                .setTradeId(sellerTradeId)
                .setPeersCustomPayoutPsbt(buyersCustomPayoutPsbt.getPsbt())
                .build());
        System.out.println("Got reply: " + sellersCustomCloseTradeResponse);
        // **********************************

        // (Buyer picks up Custom Payout Tx from bitcoin network, closing his trade.)
    }

    @SuppressWarnings("SpellCheckingInspection")
    private static ReceiverAddressAndAmount mockTradeFeeReceiver() {
        return ReceiverAddressAndAmount.newBuilder()
                .setAddress("bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k")
                .setAmount(5_000)
                .build();
    }

    @SuppressWarnings("SpellCheckingInspection")
    private static List<ReceiverAddressAndAmount> mockRedirectionReceivers(long redirectionAmountMsat) {
        // We expect the server to come up with this particular amount in millisatoshis, from the above hardcoded mock
        // trade params (that is, the given trade amount, security deposits and prepared tx feerate):
        Preconditions.checkArgument(redirectionAmountMsat == 256_115_000);

        // The mock receiver costs should add up to the above, including the contribution each output makes to the
        // Redirect Tx mining fee (which depends on the feerate and the size of the given output field in the final tx;
        // that is why millisatoshi precision is needed, as the network cost of each output is fractional in general).
        //
        // To be precise, the output-cost sum minus 'redirectionAmountMsat' should lie in the range 0..999 msat, where:
        //   output-cost := amount + output-size * feerate;
        //   output-size = 31vB for P2WPKH, 32vB for P2SH, 34vB for P2PKH, 43vB for P2TR & P2WSH.
        //
        // A one-sat-wide discrepancy range ensures fee overpay of less than 1 sat for the specified feerate, with
        // underpay never allowed, as attaining an exact feerate (in sats per kwu) is impossible in general.
        return ImmutableMap.of(
                        "bcrt1phc8m8vansnl4utths947mjquprw20puwrrdfrwx8akeeu2tqwklsnxsvf0", 160_000,
                        "bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k", 80_000,
                        "2N2x2bA28AsLZZEHss4SjFoyToQV5YYZsJM", 15_055
                )
                .entrySet().stream()
                .map(e -> ReceiverAddressAndAmount.newBuilder().setAddress(e.getKey()).setAmount(e.getValue()).build())
                .collect(Collectors.toList());
    }
}
