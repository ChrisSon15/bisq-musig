package bisq;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;

/**
 * Starts and stops a {@code testenv-server} process — a throwaway regtest {@code bitcoind}
 * (started with compact block filters and a transaction index) — for integration tests that want a chain of their own instead of one started by hand.
 *
 * <p>The binary is located via the {@code testenv.bin} system property, falling back to
 * {@code target/debug/testenv-server} relative to the workspace root. Build it first with
 * {@code cargo build -p testenv --bin testenv-server}.
 *
 * <p>testenv-server prints its connection details to stdout as {@code TESTENV_<KEY>=<value>}
 * lines (the same ones a human exports before running {@code mvn verify}), which this class
 * parses and exposes. Everything else it prints, on either stream, is kept in a log file for
 * {@link #tailLog()}. It shuts down in an orderly way on the SIGTERM that
 * {@link Process#destroy()} sends, taking bitcoind with it; {@link #close()} still
 * makes sure no daemon is left behind, which matters if the server had to be killed instead.
 */
public final class TestEnvProcess implements AutoCloseable {
    private static final long STARTUP_TIMEOUT_MS = 120_000;
    private static final long SHUTDOWN_TIMEOUT_MS = 30_000;
    /** How long bitcoind gets to finish its own shutdown before being killed. */
    private static final long DAEMON_GRACE_MS = 5_000;
    /** The settings the tests cannot do without; the server prints more, which are optional. */
    private static final List<String> REQUIRED_SETTINGS = List.of(
            "TESTENV_RPC_URL", "TESTENV_RPC_USER", "TESTENV_RPC_PASS", "TESTENV_P2P_ADDR");

    private final Process process;
    private final Path logFile;
    private final Map<String, String> settings;

    private TestEnvProcess(Process process, Path logFile, Map<String, String> settings) {
        this.process = process;
        this.logFile = logFile;
        this.settings = settings;
    }

    /** Launches testenv-server and blocks until it has printed its connection details. */
    public static TestEnvProcess start() throws IOException {
        Path binary = locateBinary();
        Path logFile = Files.createTempFile("testenv-server-", ".log");

        System.out.println("Starting testenv-server: " + binary);
        // Both streams are read by this class: the settings lines are parsed, everything else
        // (tracing goes to stdout, the server's own status lines to stderr) is kept for tailLog().
        ProcessBuilder builder = new ProcessBuilder(binary.toString())
                .redirectErrorStream(true);
        builder.environment().putIfAbsent("RUST_LOG", "info");
        Process process;
        try {
            process = builder.start();
        } catch (IOException e) {
            Files.deleteIfExists(logFile);
            throw e;
        }

        Map<String, String> settings = new ConcurrentHashMap<>();
        CountDownLatch ready = new CountDownLatch(1);
        Thread reader = new Thread(() -> readOutput(process, logFile, settings, ready),
                "testenv-server-output");
        reader.setDaemon(true);
        reader.start();

        TestEnvProcess testenv = new TestEnvProcess(process, logFile, settings);
        try {
            if (!ready.await(STARTUP_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                throw new IllegalStateException("testenv-server did not become ready within "
                        + STARTUP_TIMEOUT_MS + "ms:\n" + testenv.tailLog());
            }
            if (!process.isAlive()) {
                throw new IllegalStateException("testenv-server exited during startup (code "
                        + process.exitValue() + "):\n" + testenv.tailLog());
            }
            for (String key : REQUIRED_SETTINGS) {
                if (!settings.containsKey(key)) {
                    throw new IllegalStateException("testenv-server did not print " + key
                            + "; got " + settings.keySet() + "\n" + testenv.tailLog());
                }
            }
        } catch (InterruptedException e) {
            // Close before restoring the interrupt status: with it set, every wait inside close()
            // would throw at once and the daemons would be killed instead of shut down.
            testenv.close();
            Thread.currentThread().interrupt();
            throw new IllegalStateException("interrupted while waiting for testenv-server", e);
        } catch (RuntimeException e) {
            testenv.close();
            throw e;
        }
        System.out.println("testenv-server is up: RPC " + testenv.getRpcUrl() + ", P2P "
                + testenv.getP2pAddr());
        return testenv;
    }

    /**
     * Reads the process output: {@code TESTENV_<KEY>=<value>} lines go into {@code settings},
     * every other line into {@code logFile}. Releases {@code ready} once every required key has
     * been seen, or once the output closes (i.e. the process died).
     */
    private static void readOutput(Process process, Path logFile, Map<String, String> settings,
                                   CountDownLatch ready) {
        try (BufferedReader output = new BufferedReader(
                new InputStreamReader(process.getInputStream(), StandardCharsets.UTF_8));
             BufferedWriter log = Files.newBufferedWriter(logFile, StandardCharsets.UTF_8,
                     StandardOpenOption.WRITE, StandardOpenOption.APPEND)) {
            String line;
            while ((line = output.readLine()) != null) {
                int eq = line.indexOf('=');
                if (line.startsWith("TESTENV_") && eq > 0) {
                    settings.put(line.substring(0, eq), line.substring(eq + 1));
                    if (settings.keySet().containsAll(REQUIRED_SETTINGS)) {
                        ready.countDown();
                    }
                } else {
                    log.write(line);
                    log.newLine();
                    log.flush(); // tailLog() may be read at any moment, e.g. by a failing wait
                }
            }
        } catch (IOException ignored) {
            // The stream closes when the process goes away; the waiter checks for that.
        } finally {
            ready.countDown();
        }
    }

    private static Path locateBinary() {
        return TestProcessSupport.locateBinary("testenv.bin", "testenv-server",
                "cargo build -p testenv --bin testenv-server");
    }

    // --- settings ----------------------------------------------------------------------------

    public String getRpcUrl() {
        return settings.get("TESTENV_RPC_URL");
    }

    public String getRpcUser() {
        return settings.get("TESTENV_RPC_USER");
    }

    public String getRpcPass() {
        return settings.get("TESTENV_RPC_PASS");
    }

    /** bitcoind's P2P endpoint, the one a compact-block-filter client such as musigd syncs from. */
    public String getP2pAddr() {
        return settings.get("TESTENV_P2P_ADDR");
    }

    /** A JSON-RPC client for the node this server started. */
    public TestEnvClient newRpcClient() {
        return new TestEnvClient(getRpcUrl(), getRpcUser(), getRpcPass());
    }

    /** The last few lines testenv-server printed, on either stream, for diagnosing a failure. */
    public String tailLog() {
        return TestProcessSupport.tailLog(logFile);
    }

    // --- shutdown ----------------------------------------------------------------------------

    @Override
    public void close() {
        // Snapshot the daemons first: once testenv-server is gone they are no longer anyone's
        // descendants, and we still want to be sure they are gone too.
        List<ProcessHandle> daemons = process.toHandle().descendants().toList();
        if (process.isAlive()) {
            process.destroy(); // SIGTERM, on which testenv-server stops its daemons and exits
            try {
                if (!process.waitFor(SHUTDOWN_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                    System.out.println("testenv-server did not stop on SIGTERM; killing it");
                    process.destroyForcibly().waitFor(5, TimeUnit.SECONDS);
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                process.destroyForcibly();
            }
        }
        for (ProcessHandle daemon : daemons) {
            // An orderly testenv-server stops its daemons on the way out without waiting for
            // them, so give them a moment; a killed one never stopped them at all.
            if (!daemon.isAlive() || awaitExit(daemon, DAEMON_GRACE_MS)) {
                continue;
            }
            System.out.println("Killing a daemon testenv-server left behind: pid "
                    + daemon.pid() + " " + daemon.info().command().orElse("?"));
            daemon.destroy();
            if (!awaitExit(daemon, 10_000)) {
                daemon.destroyForcibly();
            }
        }
        try {
            Files.deleteIfExists(logFile);
        } catch (IOException ignored) {
            // Best effort — it's a temp file.
        }
    }

    /** Whether {@code handle} exited within {@code millis}. */
    private static boolean awaitExit(ProcessHandle handle, long millis) {
        try {
            handle.onExit().get(millis, TimeUnit.MILLISECONDS);
            return true;
        } catch (TimeoutException e) {
            return false;
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        } catch (ExecutionException e) {
            return true; // Already gone.
        }
    }

}
