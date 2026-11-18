package bisq;

import bisq.wallet.protobuf.GetBalanceRequest;
import bisq.wallet.protobuf.GetBalanceResponse;
import bisq.wallet.protobuf.GetUnusedAddressRequest;
import bisq.wallet.protobuf.IsWalletReadyRequest;
import bisq.wallet.protobuf.ListTransactionsRequest;
import bisq.wallet.protobuf.ListUtxosRequest;
import bisq.wallet.protobuf.OpenOrCreateWalletRequest;
import bisq.wallet.protobuf.PubAddressInfo;
import bisq.wallet.protobuf.SendToAddressRequest;
import bisq.wallet.protobuf.Transaction;
import bisq.wallet.protobuf.Utxo;
import bisq.wallet.protobuf.WalletGrpc;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;

import java.io.IOException;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.function.Supplier;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.MethodOrderer;
import org.junit.jupiter.api.Order;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestInstance;
import org.junit.jupiter.api.TestMethodOrder;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Drives the {@code wallet.Wallet} service against a real regtest chain, so the parts that a
 * wallet-less test can only smoke-test get properly exercised:
 *
 * <ul>
 *   <li>the compact-block-filter sync loop actually discovering funds,
 *   <li>{@code ListUtxos}/{@code ListTransactions}/{@code GetBalance} on a <em>funded</em> wallet
 *       rather than an empty one, and
 *   <li>{@code SendToAddress} broadcasting a transaction that bitcoind really accepts.
 * </ul>
 *
 * <p>Requires a running {@code testenv-server} (for bitcoind's RPC and P2P endpoints) and a built
 * {@code musigd} binary; this test starts and stops musigd itself. The methods are ordered
 * because they build on one another: nothing can be spent before the wallet has been funded.
 */
@TestInstance(TestInstance.Lifecycle.PER_CLASS)
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
public class BmpWalletServiceIntegrationTest {
    /** Kept clear of the default ports 50051/50052. */
    private static final int MUSIGD_PORT = 50061;
    private static final int POLL_SECONDS = 5;
    private static final long SYNC_TIMEOUT_MS = 120_000;

    private static final long FUNDING_SATS = 50_000_000; // 0.5 BTC
    private static final double FUNDING_BTC = 0.5;
    private static final long PAYMENT_SATS = 100_000;
    /** Follows the wallet\'s password rules; the wallet is a throwaway. */
    private static final String PASSWORD = "Test-Passw0rd";

    private TestEnvClient testenv;
    private MusigdProcess musigd;
    private ManagedChannel channel;
    private WalletGrpc.WalletBlockingStub stub;

    /** The address we funded, remembered across the ordered test methods. */
    private String fundedAddress;

    @BeforeAll
    void setup() throws IOException {
        testenv = TestEnvClient.fromEnv();
        System.out.println("Connected to TestEnv: " + testenv);

        String p2pAddr = TestEnvClient.p2pAddrFromEnv();
        assertNotNull(p2pAddr,
                "TESTENV_P2P_ADDR is required: the wallet syncs over compact block filters, "
                        + "not RPC. Start testenv-server and pass -DbitcoinP2pAddr=<host:port>.");

        // Regtest coinbase outputs need 100 confirmations before bitcoind will spend them.
        System.out.println("Mining 101 blocks so testenv has spendable coins...");
        testenv.mineBlocks(101);

        musigd = MusigdProcess.start(MUSIGD_PORT, testenv.getRpcUrl(), testenv.getRpcUser(),
                testenv.getRpcPass(), p2pAddr, POLL_SECONDS);

        channel = ManagedChannelBuilder.forAddress("127.0.0.1", MUSIGD_PORT).usePlaintext().build();
        stub = WalletGrpc.newBlockingStub(channel);

        // musigd no longer creates the wallet at startup: the client opens (or creates) it over
        // gRPC, supplying the wallet password.
        assertTrue(stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword(PASSWORD)
                        .build())
                        .getSuccess(),
                "OpenOrCreateWallet must succeed against a fresh wallet directory");
    }

    @AfterAll
    void tearDown() throws InterruptedException {
        if (channel != null) {
            channel.shutdown().awaitTermination(5, TimeUnit.SECONDS);
        }
        if (musigd != null) {
            musigd.close();
        }
    }

    @Test
    @Order(1)
    void walletBecomesReadyOnceItHasSyncedTheChain() {
        // `ready` only flips after the first successful sync when a chain source is configured,
        // so this is a genuine assertion about the CBF path, not just about the server being up.
        awaitTrue("the wallet to report itself ready",
                () -> stub.isWalletReady(IsWalletReadyRequest.newBuilder().build()).getReady());

        GetBalanceResponse balance = stub.getBalance(GetBalanceRequest.newBuilder().build());
        assertEquals(0, balance.getBalance(), "a fresh wallet starts empty");
    }

    @Test
    @Order(2)
    void fundingIsDiscoveredAndReportedConsistently() {
        fundedAddress = stub.getUnusedAddress(GetUnusedAddressRequest.newBuilder().build())
                .getAddress().getAddress();
        assertFalse(fundedAddress.isBlank());

        System.out.println("Funding " + fundedAddress + " with " + FUNDING_BTC + " BTC...");
        String fundingTxId = testenv.fundAddress(fundedAddress, FUNDING_BTC);
        assertTrue(testenv.waitForTransaction(fundingTxId), "bitcoind never saw the funding tx");
        testenv.mineBlocks(1);

        // The wallet has to notice this on its own, over compact block filters.
        awaitTrue("the wallet to see the funding via compact block filters",
                () -> stub.getBalance(GetBalanceRequest.newBuilder().build()).getBalance() > 0);

        GetBalanceResponse balance = stub.getBalance(GetBalanceRequest.newBuilder().build());
        assertEquals(FUNDING_SATS, balance.getBalance(), "spendable balance");
        assertEquals(FUNDING_SATS, balance.getConfirmed(), "the funding was mined, so it's confirmed");
        assertEquals(0, balance.getUntrustedPending());

        // --- ListUtxos, now with something to list ---
        List<Utxo> utxos = stub.listUtxos(ListUtxosRequest.newBuilder().build()).getUtxosList();
        assertEquals(1, utxos.size(), "exactly one funding output: " + utxos);
        Utxo utxo = utxos.get(0);
        assertEquals(fundingTxId, utxo.getTxId(), "the UTXO must point at the funding tx");
        assertEquals(FUNDING_SATS, utxo.getAmount());
        assertEquals(fundedAddress, utxo.getAddress().getAddress(),
                "the script must decode back to the address we handed out");
        assertTrue(utxo.getNumConfirmations() >= 1,
                "a mined output must have at least one confirmation, got "
                        + utxo.getNumConfirmations());

        // --- ListTransactions, now with something to list ---
        List<Transaction> txs =
                stub.listTransactions(ListTransactionsRequest.newBuilder().build())
                        .getTransactionsList();
        assertEquals(1, txs.size(), "exactly one transaction so far: " + txs);
        Transaction tx = txs.get(0);
        assertEquals(fundingTxId, tx.getTxId());
        assertTrue(tx.getIncoming(), "receiving funds must be flagged incoming");
        assertEquals(FUNDING_SATS, tx.getAmount(), "net amount credited to the wallet");
        assertTrue(tx.getBlockHeight() > 0, "a confirmed tx must carry its block height");
        assertTrue(tx.getNumConfirmations() >= 1);
        assertTrue(tx.getOutputsCount() > 0);
        assertTrue(tx.getInputsCount() > 0, "a real funding tx spends something");

        // The date bisq2 feeds to Instant.ofEpochSecond. Against a live chain we can pin this
        // properly: it must be a plausible recent wall-clock time, in seconds.
        long nowSeconds = System.currentTimeMillis() / 1000;
        assertTrue(tx.getDate() > nowSeconds - 86_400 && tx.getDate() <= nowSeconds + 3_600,
                "date must be a recent epoch-seconds value, got " + tx.getDate()
                        + " (now is " + nowSeconds + ")");
    }

    @Test
    @Order(3)
    void sendToAddressBroadcastsATransactionTheChainAccepts() {
        String target = testenv.getNewAddress();

        String txId = stub.sendToAddress(SendToAddressRequest.newBuilder()
                .setPassphrase(PASSWORD)
                .setAddress(PubAddressInfo.newBuilder().setAddress(target))
                .setAmount(PAYMENT_SATS)
                .build()).getTxId();
        assertFalse(txId.isBlank(), "a successful send must return a txid");
        System.out.println("Wallet broadcast " + txId);

        // The real check: bitcoind accepted the transaction we built, signed and published.
        assertTrue(testenv.waitForTransaction(txId),
                "bitcoind never saw the broadcast tx " + txId + "\nmusigd log:\n" + musigd.tailLog());

        testenv.mineBlocks(1);

        // ...and the wallet reports it back as an outgoing payment once it re-syncs.
        awaitTrue("the wallet to report the spend", () ->
                stub.listTransactions(ListTransactionsRequest.newBuilder().build())
                        .getTransactionsList().stream()
                        .anyMatch(tx -> tx.getTxId().equals(txId)));

        Transaction spend = stub.listTransactions(ListTransactionsRequest.newBuilder().build())
                .getTransactionsList().stream()
                .filter(tx -> tx.getTxId().equals(txId))
                .findFirst()
                .orElseThrow();

        assertFalse(spend.getIncoming(), "a payment out must not be flagged incoming");
        assertTrue(spend.getAmount() >= PAYMENT_SATS,
                "outgoing amount " + spend.getAmount() + " must cover the payment");
        assertTrue(spend.getAmount() < FUNDING_SATS, "change must not be counted as spent");

        // Balance must have dropped by the payment plus fees, and change must have come back.
        GetBalanceResponse balance = stub.getBalance(GetBalanceRequest.newBuilder().build());
        assertTrue(balance.getBalance() < FUNDING_SATS - PAYMENT_SATS,
                "balance must drop by at least the payment plus fee, got " + balance.getBalance());
        assertTrue(balance.getBalance() > 0, "the change output must still be ours");
    }

    /** Polls {@code condition} until it holds, failing with musigd's log if it never does. */
    private void awaitTrue(String what, Supplier<Boolean> condition) {
        long deadline = System.currentTimeMillis() + SYNC_TIMEOUT_MS;
        while (System.currentTimeMillis() < deadline) {
            if (condition.get()) {
                return;
            }
            try {
                Thread.sleep(1_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IllegalStateException("interrupted while waiting for " + what, e);
            }
        }
        throw new AssertionError("Timed out after " + SYNC_TIMEOUT_MS + "ms waiting for " + what
                + "\nmusigd log:\n" + musigd.tailLog());
    }
}
