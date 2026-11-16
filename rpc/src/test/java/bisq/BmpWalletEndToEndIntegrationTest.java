package bisq;

import bisq.wallet.protobuf.ChangePasswordRequest;
import bisq.wallet.protobuf.GetBalanceRequest;
import bisq.wallet.protobuf.GetBalanceResponse;
import bisq.wallet.protobuf.GetSeedWordsRequest;
import bisq.wallet.protobuf.GetUnusedAddressRequest;
import bisq.wallet.protobuf.GetWalletAddressesRequest;
import bisq.wallet.protobuf.IsWalletReadyRequest;
import bisq.wallet.protobuf.ListTransactionsRequest;
import bisq.wallet.protobuf.ListUtxosRequest;
import bisq.wallet.protobuf.OpenOrCreateWalletRequest;
import bisq.wallet.protobuf.PubAddressInfo;
import bisq.wallet.protobuf.SendToAddressRequest;
import bisq.wallet.protobuf.Transaction;
import bisq.wallet.protobuf.TransactionInput;
import bisq.wallet.protobuf.TransactionOutput;
import bisq.wallet.protobuf.Utxo;
import bisq.wallet.protobuf.WalletGrpc;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.TimeUnit;
import java.util.function.Supplier;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.MethodOrderer;
import org.junit.jupiter.api.Order;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestInstance;
import org.junit.jupiter.api.TestMethodOrder;
import org.junit.jupiter.api.function.Executable;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * The full-blown, end-to-end test of the {@code wallet.Wallet} service: every RPC that musigd
 * serves from {@code BMPWalletServiceImpl}, driven the way bisq2's {@code WalletGrpcClient}
 * would drive it, against a real regtest chain with real transactions mined into real blocks.
 *
 * <p>Unlike the other integration tests this one is self-contained: it starts its own
 * {@code testenv-server} (a regtest {@code bitcoind} with compact block filters, plus
 * {@code electrs}), mines its own chain, launches {@code musigd} against that node, and tears
 * everything down again. It needs no running testenv and no exported {@code TESTENV_*}
 * variables — only the two binaries, found via {@code -Dmusigd.bin} and {@code -Dtestenv.bin} or
 * at their default {@code target/debug} locations. See {@code readme-full-blown-wallet-test.md}.
 *
 * <p>The wallet goes through its whole life here, with three transactions along the way:
 * <ol>
 *   <li>{@code tx1}: the node funds two of the wallet's addresses in one transaction, which the
 *       wallet has to discover on its own over compact block filters;
 *   <li>{@code tx2}: the wallet pays the node at an explicit fee rate — seen unconfirmed first,
 *       then mined, with the fee checked against what bitcoind's mempool says;
 *   <li>{@code tx3}: after the password was changed and musigd restarted, the wallet pays the
 *       node again, this time spending every UTXO it has.
 * </ol>
 * In between, every RPC is exercised both on the happy path and on the refusals a client can
 * run into (wallet not open, wrong password, bad address, weak password, insufficient funds).
 * The methods are ordered because each builds on the state the previous ones left behind.
 */
@TestInstance(TestInstance.Lifecycle.PER_CLASS)
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
public class BmpWalletEndToEndIntegrationTest {
    /** Kept clear of the other integration tests' ports (50051/50052/50061/50062). */
    private static final int MUSIGD_PORT = 50063;
    /** How often musigd re-syncs the wallet; short, so the waits below don't crawl. */
    private static final int POLL_SECONDS = 2;
    private static final long SYNC_TIMEOUT_MS = 120_000;

    /** Both follow the wallet's password rules; the wallet is a throwaway. */
    private static final String PASSWORD = "End-2-End-Pa55!";
    private static final String NEW_PASSWORD = "R0tated-Pa55!";

    /** tx1 pays these two amounts to two of our addresses in one transaction. */
    private static final long FUNDING_1_SATS = 60_000_000; // 0.6 BTC
    private static final long FUNDING_2_SATS = 40_000_000; // 0.4 BTC
    private static final long FUNDED_SATS = FUNDING_1_SATS + FUNDING_2_SATS;
    /** tx2 pays this much out, at a fee rate that is deliberately not the server's default. */
    private static final long PAYMENT_1_SATS = 30_000_000; // 0.3 BTC
    private static final long PAYMENT_1_FEE_RATE_KWU = 1_000; // 4 sat/vB; the default is 500
    /** The server's default fee rate (2 sat/vB), which tx3 is expected to use. */
    private static final long DEFAULT_FEE_RATE_KWU = 500;
    /** What tx3 leaves behind for fee and change, so the payment needs every UTXO. */
    private static final long PAYMENT_2_HEADROOM_SATS = 100_000;

    /** A valid address, but for the wrong network: the wallet must refuse to pay it. */
    private static final String MAINNET_ADDRESS = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    private TestEnvProcess testenvServer;
    private TestEnvClient node;
    private Path walletDir;
    private MusigdProcess musigd;
    private ManagedChannel channel;
    private WalletGrpc.WalletBlockingStub stub;

    // State the ordered test methods hand on to one another.
    private List<String> seedWords;
    private String address1;
    private String address2;
    private String address3;
    private String fundingTxId;
    private int fundingHeight;
    private String payment1TxId;
    private long payment1FeeSats;
    private long balanceAfterPayment1;

    @BeforeAll
    void setup() throws IOException {
        testenvServer = TestEnvProcess.start();
        node = testenvServer.newRpcClient();

        // Regtest coinbase outputs need 100 confirmations before bitcoind will spend them.
        System.out.println("Mining 101 blocks so the node has spendable coins...");
        node.mineBlocks(101);

        walletDir = Files.createTempDirectory("bmp-wallet-e2e-it-");
        startMusigd();
    }

    @AfterAll
    void tearDown() throws InterruptedException {
        // Each resource is released whatever happened to the previous one: a failure in the
        // first step must not leave the regtest node running.
        try {
            stopMusigd();
        } finally {
            try {
                if (walletDir != null) {
                    MusigdProcess.deleteRecursively(walletDir);
                }
            } finally {
                if (testenvServer != null) {
                    testenvServer.close();
                }
            }
        }
    }

    // --- 1. the wallet must be opened first ------------------------------------------------

    @Test
    @Order(1)
    void everyWalletCallIsRefusedUntilTheWalletIsOpened() {
        assertFalse(ready(), "an unopened wallet must not report itself ready");

        Map<String, Executable> calls = Map.of(
                "GetBalance", this::balance,
                "GetUnusedAddress", this::unusedAddress,
                "GetWalletAddresses", this::walletAddresses,
                "ListTransactions", this::transactions,
                "ListUtxos", this::utxos,
                "GetSeedWords", () -> seedWords(PASSWORD),
                "ChangePassword", () -> changePassword(PASSWORD, NEW_PASSWORD),
                "SendToAddress", () -> send(PASSWORD, node.getNewAddress(), 1_000, null));
        calls.forEach((name, call) -> assertStatus(Status.Code.FAILED_PRECONDITION, call,
                name + " before OpenOrCreateWallet"));
    }

    // --- 2. OpenOrCreateWallet ---------------------------------------------------------------

    @Test
    @Order(2)
    void openOrCreateWalletEnforcesThePasswordRulesAndThePassword() throws IOException {
        // A password breaking the rules, the empty one included, must not create a wallet, and
        // the message must tell the user the rules.
        for (String weak : List.of("", "short", "alllowercase1!", "NoSpecialChar1")) {
            StatusRuntimeException e = assertStatus(Status.Code.INVALID_ARGUMENT,
                    () -> openOrCreate(weak), "creating a wallet with the weak password '" + weak + "'");
            assertTrue(String.valueOf(e.getStatus().getDescription()).contains("at least 8 characters"),
                    "the rules must be spelled out, got: " + e.getStatus().getDescription());
        }
        try (var files = Files.list(walletDir)) {
            assertEquals(0, files.count(), "a refused password must leave no wallet behind");
        }
        assertFalse(ready());

        assertTrue(openOrCreate(PASSWORD), "creating the wallet with an acceptable password");
        assertTrue(Files.exists(walletDir.resolve("bmp_bdk_wallet.db3")),
                "the wallet database must now exist in " + walletDir);

        // Re-opening is idempotent for a reconnecting client, but only with the right password,
        // and a wrong one must not touch the existing wallet.
        assertTrue(openOrCreate(PASSWORD), "re-opening an open wallet with its password");
        assertStatus(Status.Code.PERMISSION_DENIED, () -> openOrCreate("Wrong-Pa55word!"),
                "re-opening with a wrong password");
    }

    // --- 3. IsWalletReady, GetBalance, ListTransactions, ListUtxos on an empty wallet --------

    @Test
    @Order(3)
    void walletSyncsTheChainAndStartsEmpty() {
        // With a chain source configured, `ready` only flips after the first successful sync
        // over compact block filters, so this proves the P2P path to bitcoind works.
        awaitTrue("the wallet to finish its initial sync", this::ready);

        GetBalanceResponse balance = balance();
        assertEquals(0, balance.getBalance(), "a fresh wallet starts empty");
        assertEquals(0, balance.getConfirmed());
        assertEquals(0, balance.getTrustedPending());
        assertEquals(0, balance.getUntrustedPending());
        assertEquals(0, balance.getImmature());
        assertEquals(0, balance.getLocked());
        assertTrue(transactions().isEmpty(), "a fresh wallet has no transactions");
        assertTrue(utxos().isEmpty(), "a fresh wallet has no UTXOs");
        assertTrue(walletAddresses().isEmpty(),
                "a fresh wallet has revealed no addresses yet; GetUnusedAddress is how they appear");
    }

    // --- 4. GetSeedWords ---------------------------------------------------------------------

    @Test
    @Order(4)
    void seedWordsAreRevealedOnlyToThePasswordHolder() {
        seedWords = seedWords(PASSWORD);
        assertEquals(24, seedWords.size(), "expected a 24-word mnemonic, got " + seedWords.size());
        for (String word : seedWords) {
            assertTrue(word.matches("[a-z]+"), "a BIP39 English word, got '" + word + "'");
        }
        assertEquals(seedWords, seedWords(PASSWORD), "the seed must be stable");
        assertStatus(Status.Code.PERMISSION_DENIED, () -> seedWords("Wrong-Pa55word!"),
                "GetSeedWords with a wrong password");
    }

    // --- 5. GetUnusedAddress, GetWalletAddresses ---------------------------------------------

    @Test
    @Order(5)
    void unusedAddressesAreFreshAndListed() {
        address1 = unusedAddress();
        address2 = unusedAddress();
        assertNotEquals(address1, address2, "GetUnusedAddress must not repeat itself");
        for (String address : List.of(address1, address2)) {
            assertTrue(address.startsWith("bcrt1"), "a regtest bech32 address, got " + address);
        }

        List<String> all = walletAddresses();
        assertEquals(List.of(address1, address2), all,
                "exactly the two revealed addresses, in order");
    }

    // --- 6. tx1: funding, discovered over compact block filters ------------------------------

    @Test
    @Order(6)
    void tx1FundingWithTwoOutputsIsDiscoveredOverCompactBlockFilters() {
        System.out.println("tx1: node pays " + address1 + " and " + address2 + " in one tx");
        fundingTxId = node.sendMany(Map.of(address1, btc(FUNDING_1_SATS), address2, btc(FUNDING_2_SATS)));
        assertTrue(node.isInMempool(fundingTxId), "the node must hold tx1 in its mempool");

        // Compact block filters cover blocks, not the mempool, so the wallet learns of tx1 only
        // once it is mined. (It is deliberately not asserted that the wallet sees nothing yet.)
        node.mineBlocks(1);
        fundingHeight = node.getBlockCount();

        awaitTrue("the wallet to discover tx1 via compact block filters",
                () -> balance().getBalance() >= FUNDED_SATS);

        GetBalanceResponse balance = balance();
        assertEquals(FUNDED_SATS, balance.getBalance(), "spendable balance");
        assertEquals(FUNDED_SATS, balance.getConfirmed(), "tx1 was mined, so it is confirmed");
        assertEquals(0, balance.getTrustedPending());
        assertEquals(0, balance.getUntrustedPending());
        assertEquals(0, balance.getImmature());
        assertEquals(0, balance.getLocked());

        // --- ListUtxos: one output per funded address, both from tx1 ---
        List<Utxo> utxos = utxos();
        assertEquals(2, utxos.size(), "two funding outputs: " + utxos);
        Utxo utxo1 = utxoAt(address1);
        Utxo utxo2 = utxoAt(address2);
        assertEquals(FUNDING_1_SATS, utxo1.getAmount());
        assertEquals(FUNDING_2_SATS, utxo2.getAmount());
        for (Utxo utxo : utxos) {
            assertEquals(fundingTxId, utxo.getTxId(), "every UTXO must point at tx1");
            assertEquals(1, utxo.getNumConfirmations(), "mined in the latest block");
        }
        assertNotEquals(utxo1.getVout(), utxo2.getVout(), "distinct outputs of the same tx");

        // --- ListTransactions: tx1 in full ---
        List<Transaction> txs = transactions();
        assertEquals(1, txs.size(), "exactly one transaction so far: " + txs);
        Transaction tx = txs.get(0);
        assertEquals(fundingTxId, tx.getTxId());
        assertTrue(tx.getIncoming(), "receiving funds must be flagged incoming");
        assertEquals(FUNDED_SATS, tx.getAmount(), "the net amount credited is both outputs");
        assertEquals(fundingHeight, tx.getBlockHeight(), "mined at the current tip");
        assertEquals(1, tx.getNumConfirmations());
        assertPlausibleDate(tx.getDate());
        assertTrue(tx.getInputsCount() > 0, "a real funding tx spends something");
        for (TransactionInput input : tx.getInputsList()) {
            assertFalse(input.getPrevOutTxId().isBlank(), "inputs must name their outpoint");
            assertTrue(input.getPrevOutIndex() >= 0);
        }
        assertEquals(FUNDING_1_SATS, outputTo(tx, address1).getValue(), "our first output");
        assertEquals(FUNDING_2_SATS, outputTo(tx, address2).getValue(), "our second output");
        assertTrue(tx.getOutputsCount() >= 3, "...plus the node's change");

        // --- the node's view of the same transaction agrees ---
        JsonObject raw = node.getRawTransactionVerbose(fundingTxId);
        assertEquals(1, raw.get("confirmations").getAsInt());
        assertEquals(tx.getOutputsCount(), raw.getAsJsonArray("vout").size());

        // --- confirmations follow the tip, the block height doesn't ---
        node.mineBlocks(2);
        awaitTrue("the wallet to see two more blocks",
                () -> utxoAt(address1).getNumConfirmations() == 3);
        Transaction later = transaction(fundingTxId);
        assertEquals(3, later.getNumConfirmations());
        assertEquals(fundingHeight, later.getBlockHeight(), "the confirming block never moves");
        assertEquals(3, node.getRawTransactionVerbose(fundingTxId).get("confirmations").getAsInt());

        // --- both funded addresses are now used, so a fresh one is handed out ---
        address3 = unusedAddress();
        assertFalse(List.of(address1, address2).contains(address3),
                "a used address must not be handed out again");
        assertEquals(List.of(address1, address2, address3), walletAddresses());
    }

    // --- 7. SendToAddress: refusals, with the funds untouched --------------------------------

    @Test
    @Order(7)
    void sendToAddressRefusesBadRequestsWithoutTouchingTheFunds() {
        GetBalanceResponse before = balance();
        String target = node.getNewAddress();

        assertStatus(Status.Code.PERMISSION_DENIED,
                () -> send("Wrong-Pa55word!", target, PAYMENT_1_SATS, null),
                "SendToAddress with a wrong passphrase");

        assertStatus(Status.Code.INVALID_ARGUMENT,
                () -> stub.sendToAddress(SendToAddressRequest.newBuilder()
                        .setPassphrase(PASSWORD)
                        .setAmount(PAYMENT_1_SATS)
                        .build()),
                "SendToAddress without an address");

        StatusRuntimeException malformed = assertStatus(Status.Code.INVALID_ARGUMENT,
                () -> send(PASSWORD, "not-an-address", PAYMENT_1_SATS, null),
                "SendToAddress to a malformed address");
        assertTrue(String.valueOf(malformed.getStatus().getDescription()).contains("invalid address"),
                "the client must be told what was wrong, got: " + malformed.getStatus().getDescription());

        assertStatus(Status.Code.INVALID_ARGUMENT,
                () -> send(PASSWORD, MAINNET_ADDRESS, PAYMENT_1_SATS, null),
                "SendToAddress to an address of another network");

        StatusRuntimeException unaffordable = assertThrows(StatusRuntimeException.class,
                () -> send(PASSWORD, target, FUNDED_SATS * 2, null),
                "SendToAddress for more than the wallet holds");
        assertTrue(String.valueOf(unaffordable.getStatus().getDescription()).contains("Insufficient funds"),
                "the reason must be stated, got: " + unaffordable.getStatus());

        assertEquals(before, balance(), "a refused payment must not change the balance");
        assertEquals(2, utxos().size(), "...nor the UTXO set");
        assertEquals(1, transactions().size(), "...nor the history");
        assertEquals(0, node.rpcResult("getrawmempool", "[]").getAsJsonArray().size(),
                "no refused payment may have reached the node");
    }

    // --- 8. tx2: a payment out, unconfirmed first, then mined --------------------------------

    @Test
    @Order(8)
    void tx2PaymentIsVisibleUnconfirmedThenSettlesAtTheRequestedFeeRate() {
        String target = node.getNewAddress();
        Map<Integer, Long> fundingOutputs = Map.of(
                utxoAt(address1).getVout(), FUNDING_1_SATS,
                utxoAt(address2).getVout(), FUNDING_2_SATS);

        System.out.println("tx2: wallet pays " + PAYMENT_1_SATS + " sat to " + target);
        payment1TxId = send(PASSWORD, target, PAYMENT_1_SATS, PAYMENT_1_FEE_RATE_KWU);
        assertFalse(payment1TxId.isBlank(), "a successful send must return a txid");

        // --- (a) The wallet records its own payment before the chain confirms it ---
        Transaction spend = transaction(payment1TxId);
        assertFalse(spend.getIncoming(), "a payment out must not be flagged incoming");
        assertEquals(0, spend.getNumConfirmations(), "not mined yet");
        assertEquals(0, spend.getBlockHeight(), "no block yet");
        assertEquals(0, spend.getDate(), "no block time yet");
        assertTrue(spend.getInputsCount() >= 1);
        long spentInputs = 0;
        for (TransactionInput input : spend.getInputsList()) {
            assertEquals(fundingTxId, input.getPrevOutTxId(), "tx2 can only spend tx1's outputs");
            Long amount = fundingOutputs.get(input.getPrevOutIndex());
            assertTrue(amount != null, "unknown input " + input.getPrevOutIndex());
            assertFalse(input.getWitness().isBlank(), "a taproot spend carries a witness");
            spentInputs += amount;
        }
        assertEquals(PAYMENT_1_SATS, outputTo(spend, target).getValue(), "the payment output");
        payment1FeeSats = spend.getAmount() - PAYMENT_1_SATS;
        assertTrue(payment1FeeSats > 0, "the net amount must include a fee, got " + spend.getAmount());
        long change = spentInputs - PAYMENT_1_SATS - payment1FeeSats;
        assertTrue(change > 0, "a change output is expected, got " + change);

        GetBalanceResponse pending = balance();
        assertEquals(FUNDED_SATS - spentInputs, pending.getConfirmed(),
                "the inputs are spent, whatever tx1 output(s) remain are still confirmed");
        assertEquals(change, pending.getTrustedPending(), "our own unconfirmed change is trusted");
        assertEquals(0, pending.getUntrustedPending());
        assertEquals(0, pending.getLocked());
        assertEquals(FUNDED_SATS - PAYMENT_1_SATS - payment1FeeSats, pending.getBalance(),
                "the balance drops by the payment plus the fee, no more");

        List<Utxo> utxos = utxos();
        assertEquals(2 - spend.getInputsCount() + 1, utxos.size(),
                "the spent output(s) are gone and the change has appeared: " + utxos);
        Utxo changeUtxo = utxos.stream().filter(u -> u.getTxId().equals(payment1TxId)).findFirst()
                .orElseThrow(() -> new AssertionError("no change UTXO from tx2 in " + utxos));
        assertEquals(change, changeUtxo.getAmount());
        assertEquals(0, changeUtxo.getNumConfirmations(), "change is unconfirmed like its tx");
        String changeAddress = changeUtxo.getAddress().getAddress();
        assertTrue(walletAddresses().contains(changeAddress),
                "the change address is one of ours and so must now be listed");
        assertEquals(2, transactions().size(), "tx1 and tx2");

        // --- (b) bitcoind accepted the transaction, and charged the fee rate we asked for ---
        assertTrue(node.waitForTransaction(payment1TxId),
                "the node never saw tx2\nmusigd log:\n" + musigd.tailLog());
        JsonObject entry = node.getMempoolEntry(payment1TxId);
        assertEquals(payment1FeeSats, sats(entry.getAsJsonObject("fees").get("base")),
                "the wallet and the node must agree on the fee");
        long weight = entry.get("weight").getAsLong();
        double feeRateKwu = payment1FeeSats * 1000.0 / weight;
        assertTrue(feeRateKwu >= PAYMENT_1_FEE_RATE_KWU && feeRateKwu <= PAYMENT_1_FEE_RATE_KWU * 1.25,
                "fee rate must be the requested " + PAYMENT_1_FEE_RATE_KWU + " sat/kwu (give or take "
                        + "signature size estimation), got " + feeRateKwu);
        System.out.printf("tx2: fee %d sat for %d wu = %.1f sat/kwu%n", payment1FeeSats, weight, feeRateKwu);

        // --- (c) Mined: confirmed on both sides ---
        node.mineBlocks(1);
        int height = node.getBlockCount();
        awaitTrue("the wallet to see tx2 confirmed",
                () -> transaction(payment1TxId).getNumConfirmations() >= 1);

        Transaction mined = transaction(payment1TxId);
        assertEquals(1, mined.getNumConfirmations());
        assertEquals(height, mined.getBlockHeight());
        assertPlausibleDate(mined.getDate());
        assertEquals(spend.getAmount(), mined.getAmount(), "confirmation doesn't change the amount");

        GetBalanceResponse settled = balance();
        assertEquals(FUNDED_SATS - PAYMENT_1_SATS - payment1FeeSats, settled.getConfirmed());
        assertEquals(0, settled.getTrustedPending(), "nothing pending once mined");
        assertEquals(0, settled.getLocked());
        assertEquals(settled.getConfirmed(), settled.getBalance());
        assertEquals(1, utxoAt(changeAddress).getNumConfirmations());
        balanceAfterPayment1 = settled.getBalance();

        JsonObject received = node.getWalletTransaction(payment1TxId);
        assertEquals(PAYMENT_1_SATS, sats(received.get("amount")), "the node's wallet got paid");
        assertEquals(1, received.get("confirmations").getAsInt());
    }

    // --- 9. ChangePassword ---------------------------------------------------------------------

    @Test
    @Order(9)
    void changePasswordReKeysTheWalletWithoutTouchingTheFunds() {
        assertStatus(Status.Code.PERMISSION_DENIED,
                () -> changePassword("Wrong-Pa55word!", NEW_PASSWORD),
                "ChangePassword with a wrong old password");
        assertTrue(BmpWalletProbes.opensWith(stub, PASSWORD), "a rejected change leaves the password");

        for (String weak : List.of("", "short")) {
            StatusRuntimeException e = assertStatus(Status.Code.INVALID_ARGUMENT,
                    () -> changePassword(PASSWORD, weak), "ChangePassword to the weak password '" + weak + "'");
            assertTrue(String.valueOf(e.getStatus().getDescription()).contains("at least 8 characters"));
        }
        assertTrue(BmpWalletProbes.opensWith(stub, PASSWORD));

        assertTrue(changePassword(PASSWORD, NEW_PASSWORD), "re-keying to the new password");
        assertTrue(BmpWalletProbes.opensWith(stub, NEW_PASSWORD), "the new password is in force");
        assertFalse(BmpWalletProbes.opensWith(stub, PASSWORD), "the old one no longer is");

        assertEquals(seedWords, seedWords(NEW_PASSWORD), "the seed survives the re-key");
        assertStatus(Status.Code.PERMISSION_DENIED, () -> seedWords(PASSWORD),
                "GetSeedWords with the retired password");
        assertEquals(balanceAfterPayment1, balance().getBalance(), "the funds are untouched");
        assertStatus(Status.Code.PERMISSION_DENIED,
                () -> send(PASSWORD, node.getNewAddress(), 1_000, null),
                "SendToAddress with the retired passphrase");
    }

    // --- 10. restart: everything persists ------------------------------------------------------

    @Test
    @Order(10)
    void walletHistoryAndPasswordSurviveARestart() throws IOException, InterruptedException {
        List<String> addressesBefore = walletAddresses();
        List<Utxo> utxosBefore = utxos();

        stopMusigd();
        startMusigd();

        assertFalse(ready(), "after a restart the wallet is closed again");
        assertStatus(Status.Code.PERMISSION_DENIED, () -> openOrCreate(PASSWORD),
                "re-opening after the restart with the retired password");
        assertTrue(openOrCreate(NEW_PASSWORD), "re-opening with the password in force");
        awaitTrue("the reloaded wallet to sync", this::ready);

        assertEquals(seedWords, seedWords(NEW_PASSWORD), "the same wallet came back");
        GetBalanceResponse balance = balance();
        assertEquals(balanceAfterPayment1, balance.getBalance(), "the balance came back");
        assertEquals(balanceAfterPayment1, balance.getConfirmed());
        assertEquals(0, balance.getTrustedPending());
        assertEquals(0, balance.getLocked());
        assertEquals(Set.of(fundingTxId, payment1TxId), txIds(transactions()), "the history came back");
        assertEquals(new HashSet<>(utxosBefore), new HashSet<>(utxos()), "the UTXO set came back");
        assertEquals(addressesBefore, walletAddresses(), "the revealed addresses came back");
    }

    // --- 11. tx3: a payment after the restart, spending every UTXO ---------------------------

    @Test
    @Order(11)
    void tx3PaymentAfterRestartSpendsEveryUtxoAtTheDefaultFeeRate() {
        List<Utxo> utxosBefore = utxos();
        long total = utxosBefore.stream().mapToLong(Utxo::getAmount).sum();
        assertEquals(balanceAfterPayment1, total, "(sanity) the UTXOs add up to the balance");
        // More than any single UTXO can cover, so the wallet has to combine all of them.
        long payment = total - PAYMENT_2_HEADROOM_SATS;
        for (Utxo utxo : utxosBefore) {
            assertTrue(utxo.getAmount() < payment, "no single UTXO may cover the payment: " + utxosBefore);
        }
        String target = node.getNewAddress();

        System.out.println("tx3: wallet pays " + payment + " sat to " + target + " from "
                + utxosBefore.size() + " UTXO(s), after re-key and restart");
        String txId = send(NEW_PASSWORD, target, payment, null);

        Transaction spend = transaction(txId);
        assertFalse(spend.getIncoming());
        assertEquals(utxosBefore.size(), spend.getInputsCount(), "every UTXO is an input");
        Set<String> spent = new HashSet<>();
        for (TransactionInput input : spend.getInputsList()) {
            spent.add(input.getPrevOutTxId() + ":" + input.getPrevOutIndex());
            assertFalse(input.getWitness().isBlank(), "every input must be signed");
        }
        Set<String> owned = new HashSet<>();
        utxosBefore.forEach(u -> owned.add(u.getTxId() + ":" + u.getVout()));
        assertEquals(owned, spent, "the inputs are exactly the UTXOs the wallet had");
        assertEquals(payment, outputTo(spend, target).getValue());
        long fee = spend.getAmount() - payment;
        assertTrue(fee > 0);

        // The node accepts it, at the server's default fee rate this time.
        assertTrue(node.waitForTransaction(txId), "the node never saw tx3\nmusigd log:\n" + musigd.tailLog());
        JsonObject entry = node.getMempoolEntry(txId);
        assertEquals(fee, sats(entry.getAsJsonObject("fees").get("base")));
        double feeRateKwu = fee * 1000.0 / entry.get("weight").getAsLong();
        assertTrue(feeRateKwu >= DEFAULT_FEE_RATE_KWU && feeRateKwu <= DEFAULT_FEE_RATE_KWU * 1.25,
                "fee rate must be the default " + DEFAULT_FEE_RATE_KWU + " sat/kwu, got " + feeRateKwu);

        node.mineBlocks(1);
        awaitTrue("the wallet to see tx3 confirmed", () -> transaction(txId).getNumConfirmations() >= 1);

        // Final state: one change UTXO, three transactions, and a history that adds up.
        GetBalanceResponse balance = balance();
        long expected = total - payment - fee;
        assertEquals(expected, balance.getBalance(), "what's left after tx3");
        assertEquals(expected, balance.getConfirmed());
        assertEquals(0, balance.getTrustedPending());
        assertEquals(0, balance.getLocked());
        List<Utxo> utxos = utxos();
        assertEquals(1, utxos.size(), "only tx3's change remains: " + utxos);
        assertEquals(txId, utxos.get(0).getTxId());
        assertEquals(expected, utxos.get(0).getAmount());
        assertEquals(1, utxos.get(0).getNumConfirmations());

        List<Transaction> history = transactions();
        assertEquals(Set.of(fundingTxId, payment1TxId, txId), txIds(history));
        long net = 0;
        for (Transaction tx : history) {
            assertTrue(tx.getNumConfirmations() >= 1, "everything is mined by now: " + tx.getTxId());
            net += tx.getIncoming() ? tx.getAmount() : -tx.getAmount();
        }
        assertEquals(expected, net, "credits minus debits must equal the balance");

        assertEquals(payment, sats(node.getWalletTransaction(txId).get("amount")), "the node got paid");
        System.out.println("Final balance: " + expected + " sat after 3 transactions");
    }

    // --- RPC wrappers ------------------------------------------------------------------------

    private boolean ready() {
        return stub.isWalletReady(IsWalletReadyRequest.newBuilder().build()).getReady();
    }

    private boolean openOrCreate(String password) {
        return stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                .setPassword(password)
                .build()).getSuccess();
    }

    private GetBalanceResponse balance() {
        return stub.getBalance(GetBalanceRequest.newBuilder().build());
    }

    private String unusedAddress() {
        return stub.getUnusedAddress(GetUnusedAddressRequest.newBuilder().build())
                .getAddress().getAddress();
    }

    private List<String> walletAddresses() {
        return stub.getWalletAddresses(GetWalletAddressesRequest.newBuilder().build())
                .getAddressesList().stream().map(PubAddressInfo::getAddress).toList();
    }

    private List<Transaction> transactions() {
        return stub.listTransactions(ListTransactionsRequest.newBuilder().build()).getTransactionsList();
    }

    private List<Utxo> utxos() {
        return stub.listUtxos(ListUtxosRequest.newBuilder().build()).getUtxosList();
    }

    private List<String> seedWords(String password) {
        return stub.getSeedWords(GetSeedWordsRequest.newBuilder().setPassword(password).build())
                .getSeedWordsList();
    }

    private boolean changePassword(String oldPassword, String newPassword) {
        return stub.changePassword(ChangePasswordRequest.newBuilder()
                .setOldPassword(oldPassword)
                .setNewPassword(newPassword)
                .build()).getSuccess();
    }

    private String send(String passphrase, String address, long sats, Long feeRateKwu) {
        SendToAddressRequest.Builder request = SendToAddressRequest.newBuilder()
                .setPassphrase(passphrase)
                .setAddress(PubAddressInfo.newBuilder().setAddress(address))
                .setAmount(sats);
        if (feeRateKwu != null) {
            request.setFeeRatePerKwu(feeRateKwu);
        }
        return stub.sendToAddress(request.build()).getTxId();
    }

    // --- lookups -----------------------------------------------------------------------------

    private Transaction transaction(String txId) {
        List<Transaction> txs = transactions();
        return txs.stream().filter(tx -> tx.getTxId().equals(txId)).findFirst()
                .orElseThrow(() -> new AssertionError("no transaction " + txId + " in " + txIds(txs)));
    }

    private Utxo utxoAt(String address) {
        List<Utxo> utxos = utxos();
        return utxos.stream().filter(u -> u.getAddress().getAddress().equals(address)).findFirst()
                .orElseThrow(() -> new AssertionError("no UTXO at " + address + " in " + utxos));
    }

    private static TransactionOutput outputTo(Transaction tx, String address) {
        return tx.getOutputsList().stream()
                .filter(o -> o.hasAddress() && o.getAddress().getAddress().equals(address))
                .findFirst()
                .orElseThrow(() -> new AssertionError("no output to " + address + " in " + tx));
    }

    private static Set<String> txIds(List<Transaction> txs) {
        Set<String> ids = new HashSet<>();
        txs.forEach(tx -> ids.add(tx.getTxId()));
        return ids;
    }

    // --- assertions --------------------------------------------------------------------------

    private static StatusRuntimeException assertStatus(Status.Code expected, Executable call, String what) {
        StatusRuntimeException e = assertThrows(StatusRuntimeException.class, call,
                what + " must fail with " + expected);
        assertEquals(expected, e.getStatus().getCode(), what + ": " + e.getStatus());
        return e;
    }

    /**
     * The date bisq2 feeds to {@code Instant.ofEpochSecond}: against a live chain it must be a
     * recent wall-clock time in seconds (regtest block times may run a little ahead when blocks
     * are mined in quick succession, hence the slack).
     */
    private static void assertPlausibleDate(long date) {
        long now = System.currentTimeMillis() / 1000;
        assertTrue(date > now - 86_400 && date <= now + 7_200,
                "date must be a recent epoch-seconds value, got " + date + " (now is " + now + ")");
    }

    /** Polls {@code condition} until it holds, failing with musigd's log if it never does. */
    private void awaitTrue(String what, Supplier<Boolean> condition) {
        long deadline = System.currentTimeMillis() + SYNC_TIMEOUT_MS;
        while (System.currentTimeMillis() < deadline) {
            if (condition.get()) {
                return;
            }
            try {
                Thread.sleep(500);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IllegalStateException("interrupted while waiting for " + what, e);
            }
        }
        throw new AssertionError("Timed out after " + SYNC_TIMEOUT_MS + "ms waiting for " + what
                + "\nmusigd log:\n" + musigd.tailLog()
                + "\ntestenv-server log:\n" + testenvServer.tailLog());
    }

    // --- conversions -------------------------------------------------------------------------

    private static double btc(long sats) {
        return sats / 100_000_000.0;
    }

    /** A BTC amount as bitcoind reports it (e.g. {@code 0.00000616}), in satoshis. */
    private static long sats(JsonElement btc) {
        return btc.getAsBigDecimal().movePointRight(8).longValueExact();
    }

    // --- plumbing ----------------------------------------------------------------------------

    private void startMusigd() throws IOException {
        musigd = MusigdProcess.start(MUSIGD_PORT, node.getRpcUrl(), node.getRpcUser(),
                node.getRpcPass(), testenvServer.getP2pAddr(), POLL_SECONDS, walletDir);
        channel = ManagedChannelBuilder.forAddress("127.0.0.1", MUSIGD_PORT).usePlaintext().build();
        stub = WalletGrpc.newBlockingStub(channel);
    }

    private void stopMusigd() throws InterruptedException {
        try {
            if (channel != null) {
                channel.shutdown().awaitTermination(5, TimeUnit.SECONDS);
            }
        } finally {
            channel = null;
            if (musigd != null) {
                musigd.close();
                musigd = null;
            }
        }
    }
}
