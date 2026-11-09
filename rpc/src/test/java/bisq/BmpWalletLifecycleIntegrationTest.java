package bisq;

import bisq.wallet.protobuf.ChangePasswordRequest;
import bisq.wallet.protobuf.GetBalanceRequest;
import bisq.wallet.protobuf.GetSeedWordsRequest;
import bisq.wallet.protobuf.GetUnusedAddressRequest;
import bisq.wallet.protobuf.IsWalletReadyRequest;
import bisq.wallet.protobuf.IsWalletReadyResponse;
import bisq.wallet.protobuf.OpenOrCreateWalletRequest;
import bisq.wallet.protobuf.WalletGrpc;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.TimeUnit;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.MethodOrderer;
import org.junit.jupiter.api.Order;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestInstance;
import org.junit.jupiter.api.TestMethodOrder;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Drives the wallet-lifecycle RPCs of the {@code wallet.Wallet} service against a real musigd
 * process: {@code OpenOrCreateWallet} (which replaced the {@code --wallet-password} command-line
 * argument) and {@code ChangePassword} (which replaced {@code EncryptWallet}/{@code
 * DecryptWallet}).
 *
 * <p>Everything under test is pure wallet-database state, so unlike
 * {@link BmpWalletServiceIntegrationTest} this needs <em>no</em> running testenv or chain
 * backend — only a built {@code musigd} binary. The wallet directory is owned by the test so
 * musigd can be stopped and restarted against the same wallet, proving the password state
 * persists. The methods are ordered because they build on one another.
 */
@TestInstance(TestInstance.Lifecycle.PER_CLASS)
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
public class BmpWalletLifecycleIntegrationTest {
    /** Kept clear of the other integration tests' ports (50051/50052/50061). */
    private static final int MUSIGD_PORT = 50062;
    /** Nothing listens here: the daemon must work without a reachable Bitcoin Core. */
    private static final String DUMMY_RPC_URL = "http://127.0.0.1:1";

    /** Both follow the wallet's password rules. */
    private static final String PASSWORD = "S3cret!!";
    private static final String NEW_PASSWORD = "N3w-Pass!";

    private Path walletDir;
    private MusigdProcess musigd;
    private ManagedChannel channel;
    private WalletGrpc.WalletBlockingStub stub;

    /** The wallet's seed, remembered to prove restarts and re-keys preserve the same wallet. */
    private List<String> seedWords;

    @BeforeAll
    void setup() throws IOException {
        walletDir = Files.createTempDirectory("bmp-wallet-lifecycle-it-");
        startMusigd();
    }

    @AfterAll
    void tearDown() throws InterruptedException {
        stopMusigd();
        if (walletDir != null) {
            MusigdProcess.deleteRecursively(walletDir);
        }
    }

    @Test
    @Order(1)
    void walletOperationsBeforeOpenAreRefused() {
        IsWalletReadyResponse status = walletStatus();
        assertFalse(status.getReady(), "an unopened wallet must not report itself ready");
        assertTrue(status.hasExists() && status.hasOpen(),
                "musigd must report whether the wallet exists and is open");
        assertFalse(status.getExists(), "no wallet has been created yet");
        assertFalse(status.getOpen());

        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> stub.getBalance(GetBalanceRequest.newBuilder().build()));
        assertEquals(Status.Code.FAILED_PRECONDITION, e.getStatus().getCode(),
                "wallet operations before OpenOrCreateWallet must fail as a precondition error");

        e = assertThrows(StatusRuntimeException.class,
                () -> stub.getUnusedAddress(GetUnusedAddressRequest.newBuilder().build()));
        assertEquals(Status.Code.FAILED_PRECONDITION, e.getStatus().getCode());
    }

    @Test
    @Order(2)
    void openOrCreateWalletCreatesAFreshProtectedWallet() {
        // A password breaking the rules, the empty one included, must not create a wallet, and
        // the user must be told the rules.
        for (String weak : List.of("", "short")) {
            StatusRuntimeException weakErr = assertThrows(StatusRuntimeException.class,
                    () -> stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                            .setPassword(weak)
                            .build()));
            assertEquals(Status.Code.INVALID_ARGUMENT, weakErr.getStatus().getCode(),
                    "a password breaking the rules must be INVALID_ARGUMENT, got: " + weakErr);
            String description = String.valueOf(weakErr.getStatus().getDescription());
            assertTrue(description.contains("at least 8 characters"),
                    "the rules must be spelled out, got: " + description);
        }
        assertFalse(walletStatus().getExists(), "no wallet must have been created");

        assertTrue(stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword(PASSWORD)
                        .build())
                .getSuccess());

        IsWalletReadyResponse status = walletStatus();
        assertTrue(status.getReady(),
                "with no chain source configured, an open wallet is a ready wallet");
        assertTrue(status.getExists(), "the created wallet must exist");
        assertTrue(status.getOpen(), "the created wallet must be open");
        assertEquals(0, stub.getBalance(GetBalanceRequest.newBuilder().build()).getBalance(),
                "a fresh wallet starts empty");

        seedWords = seedWords(PASSWORD);
        assertEquals(24, seedWords.size(), "expected a 24-word mnemonic");

        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> seedWords("wrong"));
        assertEquals(Status.Code.PERMISSION_DENIED, e.getStatus().getCode(),
                "the seed must only be revealed to the holder of the password");
    }

    @Test
    @Order(3)
    void reopeningIsIdempotentButChecksThePassword() {
        // Same password: fine (e.g. a client reconnecting).
        assertTrue(stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword(PASSWORD)
                        .build())
                .getSuccess());

        // Wrong password: rejected, and crucially the existing wallet must survive untouched.
        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword("wrong")
                        .build()));
        assertEquals(Status.Code.PERMISSION_DENIED, e.getStatus().getCode());
        assertEquals(seedWords, seedWords(PASSWORD), "the open wallet must be unaffected");
    }

    @Test
    @Order(4)
    void changePasswordRequiresTheCurrentPassword() {
        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> stub.changePassword(ChangePasswordRequest.newBuilder()
                        .setOldPassword("definitely-wrong")
                        .setNewPassword("irrelevant")
                        .build()));
        assertEquals(Status.Code.PERMISSION_DENIED, e.getStatus().getCode(),
                "a wrong old password must be rejected as PERMISSION_DENIED");
        assertTrue(BmpWalletProbes.opensWith(stub, PASSWORD),
                "a rejected change must leave the wallet as it was");
        assertEquals(seedWords, seedWords(PASSWORD), "...and the key must not have rotated");
    }

    @Test
    @Order(5)
    void changePasswordReKeysTheWallet() {
        assertTrue(stub.changePassword(ChangePasswordRequest.newBuilder()
                        .setOldPassword(PASSWORD)
                        .setNewPassword(NEW_PASSWORD)
                        .build())
                .getSuccess());
        assertTrue(BmpWalletProbes.opensWith(stub, NEW_PASSWORD),
                "the new password must now be the one in force");
        assertFalse(BmpWalletProbes.opensWith(stub, PASSWORD),
                "the old password must no longer open the wallet");
        assertEquals(seedWords, seedWords(NEW_PASSWORD), "the seed must survive the re-key");

        // The wallet is always password-protected: an empty new password (the former
        // DecryptWallet) is refused like any other password breaking the rules, and the current
        // one stays in force.
        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> stub.changePassword(ChangePasswordRequest.newBuilder()
                        .setOldPassword(NEW_PASSWORD)
                        .build()));
        assertEquals(Status.Code.INVALID_ARGUMENT, e.getStatus().getCode(),
                "an empty new password must be rejected as INVALID_ARGUMENT");
        assertTrue(BmpWalletProbes.opensWith(stub, NEW_PASSWORD),
                "a rejected change must leave the password as it was");
    }

    @Test
    @Order(6)
    void walletAndPasswordStatePersistAcrossARestart() throws IOException, InterruptedException {
        stopMusigd();
        startMusigd();

        // Before any password is sent, the client can tell the wallet is there but closed, so
        // OpenOrCreateWallet would open it rather than create a new one.
        IsWalletReadyResponse status = walletStatus();
        assertTrue(status.getExists(), "the persisted wallet must be seen without a password");
        assertFalse(status.getOpen(), "the restarted daemon has not opened the wallet yet");
        assertFalse(status.getReady());

        // The daemon restarted, so the wallet must be re-opened — and the password in force is
        // the one the previous test changed to, not the one the wallet was created with.
        StatusRuntimeException e = assertThrows(StatusRuntimeException.class,
                () -> stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword(PASSWORD)
                        .build()));
        assertEquals(Status.Code.PERMISSION_DENIED, e.getStatus().getCode(),
                "a stale password must not open the wallet — and must not overwrite it");

        assertTrue(stub.openOrCreateWallet(OpenOrCreateWalletRequest.newBuilder()
                        .setPassword(NEW_PASSWORD)
                        .build())
                .getSuccess());
        assertEquals(seedWords, seedWords(NEW_PASSWORD),
                "reloading must yield the same wallet, not a fresh one");
        assertTrue(walletStatus().getOpen());
    }

    // --- plumbing ----------------------------------------------------------------------------

    private void startMusigd() throws IOException {
        musigd = MusigdProcess.start(MUSIGD_PORT, DUMMY_RPC_URL, "user", "pass",
                null, 1, walletDir);
        channel = ManagedChannelBuilder.forAddress("127.0.0.1", MUSIGD_PORT)
                .usePlaintext()
                .build();
        stub = WalletGrpc.newBlockingStub(channel);
    }

    private void stopMusigd() throws InterruptedException {
        if (channel != null) {
            channel.shutdown().awaitTermination(5, TimeUnit.SECONDS);
            channel = null;
        }
        if (musigd != null) {
            musigd.close();
            musigd = null;
        }
    }

    private IsWalletReadyResponse walletStatus() {
        return stub.isWalletReady(IsWalletReadyRequest.newBuilder().build());
    }

    private List<String> seedWords(String password) {
        return stub.getSeedWords(GetSeedWordsRequest.newBuilder().setPassword(password).build())
                .getSeedWordsList();
    }
}
