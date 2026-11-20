"""Generates test-musigd-sequence-diagrams.md: one sequence diagram per test of tests/test_musigd.rs.

The diagrams are written in Mermaid. The script does not read the test code: every function
`test_1` to `test_10` describes one test by hand, and the helper functions follow the helper
functions of the test file (`funded_daemons`, `set_up_trade`, `wait_for_state`, ...). When a test
changes, change its description here, set the date in `HEADER`, and run the script again:

    python3 rpc/test-musigd-sequence-diagrams.py

The script has no dependencies. It writes the Markdown file next to itself, or to the path that
is given as its argument.
"""
import pathlib
import sys
import textwrap


def wrap(text, width):
    """Breaks the lines of a label at spaces, so that the diagram stays narrow."""
    text = text.replace("<br/>", " ")
    lines = textwrap.wrap(text, width, break_long_words=False, break_on_hyphens=False)
    return "<br/>".join(lines)


LABEL_WIDTH = 36
NOTE_WIDTH = 30
WIDE_NOTE_WIDTH = 90

BD, B, S, SD, N = "BD", "B", "S", "SD", "N"
ALL = f"{BD},{N}"
TRADERS = f"{B},{S}"


class Diagram:
    def __init__(self, participants):
        self.lines = ["sequenceDiagram", "    autonumber"]
        for alias, name in participants:
            self.lines.append(f"    participant {alias} as {name}")
        self.indent = 1

    def add(self, text):
        self.lines.append("    " * self.indent + text)

    def arrow(self, sender, arrow, receiver, text):
        self.add(f"{sender}{arrow}{receiver}: {wrap(text, LABEL_WIDTH)}")

    def call(self, client, daemon, text):
        self.arrow(client, "->>", daemon, text)

    def answer(self, daemon, client, text):
        self.arrow(daemon, "-->>", client, text)

    def refused(self, daemon, client, text):
        self.arrow(daemon, "--x", client, text)

    def message(self, sender, receiver, text):
        self.arrow(sender, "->>", receiver, text)

    def lost(self, sender, receiver, text):
        self.arrow(sender, "-x", receiver, text)

    def p2p(self, daemon, text):
        self.arrow(daemon, "->>", N, f"P2P: {text}")

    def note(self, over, text):
        width = WIDE_NOTE_WIDTH if "," in over else NOTE_WIDTH
        self.add(f"Note over {over}: {wrap(text, width)}")

    def state(self, daemon, state, checked=False):
        self.note(daemon, f"State: {state}" + (" (checked)" if checked else ""))

    def part(self, text, over=ALL):
        self.note(over, f"Part: {text}")

    def loop(self, text):
        self.add(f"loop {text}")
        self.indent += 1

    def opt(self, text):
        self.add(f"opt {text}")
        self.indent += 1

    def end(self):
        self.indent -= 1
        self.add("end")

    def render(self):
        return "```mermaid\n" + "\n".join(self.lines) + "\n```\n"


TWO = [
    (BD, "Buyer's musigd"),
    (B, "Buyer's bisq2 (test)"),
    (S, "Seller's bisq2 (test)"),
    (SD, "Seller's musigd"),
    (N, "bitcoind (regtest)"),
]


def side(buyer):
    return (B, BD, "buyer") if buyer else (S, SD, "seller")


def open_wallet(d, buyer):
    client, daemon, _ = side(buyer)
    d.call(client, daemon, "Wallet.OpenOrCreateWallet(password)")
    d.answer(daemon, client, "ok")


def start_daemon(d, buyer, timeout=None):
    client, daemon, _ = side(buyer)
    extra = f", deposit timeout {timeout}" if timeout else ""
    d.note(
        daemon,
        f"The test starts musigd: new wallet directory,<br/>peer = bitcoind, warning lock time 20,<br/>claim lock time 10{extra}",
    )
    open_wallet(d, buyer)


def stop(d, buyer):
    _, daemon, _ = side(buyer)
    d.note(daemon, "The test ends the process at once (kill)")


def restart(d, buyer, running=True):
    _, daemon, _ = side(buyer)
    if running:
        stop(d, buyer)
    d.note(daemon, "The test starts musigd again<br/>on the same wallet directory")
    open_wallet(d, buyer)


def balance_until(d, buyer, what):
    client, daemon, _ = side(buyer)
    d.loop(f"until the balance is {what}")
    d.call(client, daemon, "Wallet.GetBalance()")
    d.answer(daemon, client, "balance")
    d.end()


def trade_state(d, buyer, state, trade="trade-1", checked=True, extra=""):
    client, daemon, _ = side(buyer)
    d.call(client, daemon, f"Trade.SubscribeTradeEvents({trade})")
    d.answer(daemon, client, f"first event: STATE{extra}")
    d.state(daemon, state, checked)


def wait_for_state(d, buyer, state, condition=None, mine=False, extra=""):
    client, daemon, _ = side(buyer)
    d.loop("until the first event reports " + (condition or state))
    if mine:
        d.note(N, "The test mines 1 block")
    d.call(client, daemon, "Trade.SubscribeTradeEvents(trade-1)")
    d.answer(daemon, client, f"first event: STATE{extra}")
    d.end()
    d.state(daemon, state, True)


def env(d):
    d.part("start and funding")
    d.note(N, "The test starts bitcoind<br/>and mines 101 blocks")


def funded_daemons(d, timeout=None):
    env(d)
    start_daemon(d, True, timeout)
    start_daemon(d, False, timeout)
    for buyer in (True, False):
        client, daemon, _ = side(buyer)
        d.call(client, daemon, "Wallet.GetUnusedAddress()")
        d.answer(daemon, client, "address")
        d.note(N, "The test pays 1 BTC<br/>to this address (RPC)")
    d.note(N, "The test mines 1 block")
    for buyer in (True, False):
        balance_until(d, buyer, "1 BTC")


def set_up_trade(d):
    d.part("setup of the trade trade-1, messages A to D")
    d.call(B, BD, "Trade.InitTrade(trade-1, BUYER_AS_TAKER)")
    d.answer(BD, B, "buyer's public key shares, script key")
    d.state(BD, "Init")
    d.call(S, SD, "Trade.InitTrade(trade-1, SELLER_AS_MAKER)")
    d.answer(SD, S, "seller's public key shares, script key")
    d.state(SD, "Init")
    d.message(B, S, "Message A: buyer's public key shares, script key")
    d.call(S, SD, "Trade.GetNonceShares(buyer's keys, trade parameters)")
    d.answer(SD, S, "seller's half deposit, addresses, nonces,<br/>redirection amount")
    d.message(S, B, "Message B: seller's public key shares, script key,<br/>half deposit, addresses, nonces")
    d.call(B, BD, "Trade.GetNonceShares(seller's keys, trade parameters)")
    d.answer(BD, B, "buyer's half deposit, addresses, nonces,<br/>redirection amount")
    d.call(B, BD, "Wallet.GetBalance()")
    d.answer(BD, B, "0: the coins of the deposit are reserved")
    d.call(S, SD, "Wallet.GetBalance()")
    d.answer(SD, S, "0: the coins of the deposit are reserved")
    d.note(
        TRADERS,
        "The test builds one redirection receiver:<br/>a new address of bitcoind (RPC) gets the whole redirection amount",
    )
    d.call(B, BD, "Trade.GetPartialSignatures(seller's half deposit, addresses, nonces,<br/>redirection receivers, buyerReadyToRelease = false)")
    d.answer(BD, B, "buyer's partial signatures, IDs of the 5 transactions")
    d.message(B, S, "Message C: buyer's half deposit, addresses, nonces,<br/>partial signatures")
    d.call(S, SD, "Trade.GetPartialSignatures(buyer's half deposit, addresses, nonces,<br/>redirection receivers, buyerReadyToRelease = false)")
    d.answer(SD, S, "seller's partial signatures, IDs of the 5 transactions")
    d.call(S, SD, "Trade.SignDepositTx(buyer's partial signatures)")
    d.answer(SD, S, "deposit PSBT with the seller's signatures")
    d.state(SD, "Deposit")
    d.message(S, B, "Message D: seller's partial signatures,<br/>deposit PSBT with the seller's signatures")
    d.call(B, BD, "Trade.SignDepositTx(seller's partial signatures)")
    d.answer(BD, B, "deposit PSBT with the buyer's signatures")
    d.state(BD, "Deposit")


def publish_and_confirm(d):
    d.part("deposit transaction")
    d.call(B, BD, "Trade.PublishDepositTx(seller's deposit PSBT)")
    d.p2p(BD, "deposit transaction")
    d.loop("until the stream ends, at 3 confirmations")
    d.note(N, "The test mines 1 block")
    d.answer(BD, B, "TxConfirmationStatus(numConfirmations)")
    d.end()
    d.note(f"{BD},{SD}", "State of both trades: Deposit. The deposit transaction has 3 confirmations.")


def start_payment(d):
    d.call(B, BD, "Trade.GetPartialSignatures(buyerReadyToRelease = true)")
    d.answer(BD, B, "buyer's partial signature for the swap transaction")


def release(d, deliver_f):
    d.part("payment started and release, messages E and F")
    start_payment(d)
    d.state(BD, "BuyerReadyToRelease")
    d.message(B, S, "Message E: buyer's partial signature for the swap transaction")
    d.call(S, SD, "Trade.SignSwapTx(buyer's partial signature,<br/>sellerReadyToRelease = false)")
    d.answer(SD, S, "response without a key share")
    d.state(SD, "BuyerReadyToRelease")
    d.call(S, SD, "Trade.SignSwapTx(no signature, sellerReadyToRelease = true)")
    d.answer(SD, S, "seller's secret key share for the buyer's payout")
    d.state(SD, "SellerReadyToRelease")
    if deliver_f:
        d.message(S, B, "Message F: seller's secret key share for the buyer's payout")
    else:
        d.lost(S, B, "Message F is not delivered: the buyer does not answer")


def participants_single(client, daemon):
    return [("C", client), ("D", daemon)]


def test_1():
    d = Diagram(participants_single("bisq2 (test)", "musigd"))
    d.note("D", "The test starts musigd: new wallet directory, no peer,<br/>warning lock time 20, claim lock time 10")
    d.call("C", "D", "Wallet.IsWalletReady()")
    d.answer("D", "C", "ready = false")
    d.call("C", "D", "Trade.InitTrade(musigd-smoke-test, BUYER_AS_TAKER)")
    d.refused("D", "C", "FAILED_PRECONDITION: no wallet is open")
    d.note("D", "No trade exists")
    d.call("C", "D", "Wallet.OpenOrCreateWallet(password)")
    d.answer("D", "C", "ok: the wallet is created")
    d.call("C", "D", "Trade.InitTrade(musigd-smoke-test, BUYER_AS_TAKER)")
    d.answer("D", "C", "public key shares (33 bytes each), script key (32 bytes)")
    d.note("D", "State: Init")
    return d


def test_2():
    d = Diagram([("T", "Test"), ("D", "musigd")])
    for variable, value, error in [
        ("BISQ_MUSIG_WARNING_LOCK_TIME", "9", "warning lock time of 9 blocks"),
        ("BISQ_MUSIG_CLAIM_LOCK_TIME", "1", "claim lock time of 1 blocks"),
        ("BISQ_MUSIG_WARNING_LOCK_TIME", "soon", "invalid value"),
    ]:
        d.message("T", "D", f"start the process with {variable} = {value}")
        d.refused("D", "T", f"the process ends with an error status,<br/>error output contains: {error}")
    d.note("T,D", "musigd serves no request. No wallet is opened and no trade exists.")
    return d


def test_3():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    d.part("deposit transaction")
    d.call(S, SD, "Trade.SubscribeTxConfirmationStatus(trade-1)")
    d.call(B, BD, "Trade.PublishDepositTx(seller's deposit PSBT)")
    d.p2p(BD, "deposit transaction")
    d.loop("until the first event arrives")
    d.note(N, "The test mines 1 block")
    d.end()
    d.answer(BD, B, "TxConfirmationStatus(transaction, height, 1 confirmation)")
    d.note(N, "The test mines 2 blocks")
    d.loop("until the stream ends, at 3 confirmations")
    d.answer(BD, B, "TxConfirmationStatus(numConfirmations)")
    d.end()
    d.loop("until the stream ends, at 3 confirmations")
    d.answer(SD, S, "TxConfirmationStatus(transaction from the block, numConfirmations)")
    d.end()
    d.note(f"{BD},{SD}", "State of both trades: Deposit. The deposit transaction has 3 confirmations.")
    release(d, deliver_f=True)
    d.part("cooperative close, message G")
    d.call(B, BD, "Trade.CloseTrade(seller's secret key share)")
    d.answer(BD, B, "buyer's secret key share for the seller's payout")
    d.state(BD, "TradeClosed(Cooperative)")
    d.message(B, S, "Message G: buyer's secret key share for the seller's payout")
    d.call(S, SD, "Trade.CloseTrade(buyer's secret key share)")
    d.answer(SD, S, "response without a key share")
    d.state(SD, "TradeClosed(Cooperative)")
    d.part("payouts in the wallets")
    balance_until(d, True, "1 BTC + trade amount, less fees")
    balance_until(d, False, "1 BTC - trade amount, less fees")
    d.part("drain of the buyer's payout")
    d.call(B, BD, "Wallet.Close()")
    d.p2p(BD, "drain transaction")
    d.answer(BD, B, "drainTxId")
    d.loop("until the balance is lower by the fee of the drain transaction only")
    d.note(N, "The test mines 1 block")
    d.call(B, BD, "Wallet.GetBalance()")
    d.answer(BD, B, "balance")
    d.end()
    return d


def subscribe_events(d, buyer):
    client, daemon, _ = side(buyer)
    d.call(client, daemon, "Trade.SubscribeTradeEvents(trade-1)")
    d.answer(daemon, client, "first event: STATE")


def swap_sent_by_seller(d):
    d.loop("until the seller's stream reports TX_SENT")
    d.note(N, "The test mines 1 block")
    d.opt("the deposit has 14 confirmations")
    d.note(SD, "6 blocks before the buyer's warning becomes valid:<br/>musigd closes by force on its own")
    d.p2p(SD, "swap transaction")
    d.answer(SD, S, "event: TX_SENT, SWAP_TX,<br/>state TRADE_CLOSED, FORCED")
    d.end()
    d.end()


def payouts_after_forced_close(d):
    d.part("payouts in the wallets")
    balance_until(d, True, "1 BTC + trade amount, less fees")
    balance_until(d, False, "1 BTC - trade amount, less fees")


def test_4():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    publish_and_confirm(d)
    release(d, deliver_f=False)
    d.part("forced close by the seller's musigd")
    subscribe_events(d, True)
    subscribe_events(d, False)
    swap_sent_by_seller(d)
    d.state(SD, "TradeClosed(Forced)", True)
    d.loop("until the buyer's stream reports STATE TRADE_CLOSED")
    d.note(N, "The test mines 1 block")
    d.opt("the swap transaction is confirmed")
    d.note(BD, "musigd reads the seller's key share from the<br/>swap transaction and imports the payout")
    d.answer(BD, B, "event: STATE, TRADE_CLOSED, FORCED")
    d.end()
    d.end()
    d.state(BD, "TradeClosed(Forced)", True)
    payouts_after_forced_close(d)
    return d


def test_5():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    publish_and_confirm(d)
    release(d, deliver_f=False)
    d.part("restart of both daemons")
    restart(d, True)
    restart(d, False)
    trade_state(d, True, "BuyerReadyToRelease")
    trade_state(d, False, "SellerReadyToRelease")
    d.loop("until the request is accepted")
    d.call(S, SD, "Trade.SignSwapTx(no signature, sellerReadyToRelease = true)")
    d.end()
    d.answer(SD, S, "seller's secret key share again")
    d.call(B, BD, "Trade.InitTrade(trade-1, BUYER_AS_TAKER)")
    d.refused(BD, B, "ALREADY_EXISTS: the trade ID is used")
    d.part("forced close while the buyer's musigd does not run")
    stop(d, True)
    subscribe_events(d, False)
    swap_sent_by_seller(d)
    d.state(SD, "TradeClosed(Forced)")
    d.note(N, "The test mines 4 blocks:<br/>the swap transaction is confirmed")
    restart(d, True, running=False)
    d.call(B, BD, "Trade.SubscribeTradeEvents(trade-1)")
    d.note(BD, "musigd evaluates the blocks that it missed,<br/>reads the seller's key share from the swap<br/>transaction and imports the payout")
    d.loop("until an event reports STATE TRADE_CLOSED")
    d.answer(BD, B, "event")
    d.end()
    d.state(BD, "TradeClosed(Forced)", True)
    payouts_after_forced_close(d)
    return d


def publish_warning(d, again=False):
    d.loop("until the request is accepted")
    d.call(B, BD, "Trade.PublishWarningTx(trade-1)")
    d.end()
    d.answer(BD, B, "the same warning transaction" if again else "buyer's warning transaction")


def warning_confirmed(d):
    d.call(B, BD, "Trade.SubscribeTradeEvents(trade-1)")
    d.answer(BD, B, "first event: STATE")
    d.loop("until the buyer's stream reports TX_CONFIRMED for the warning")
    d.note(N, "The test mines 1 block")
    d.end()


def test_6():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    publish_and_confirm(d)
    d.part("restart of both daemons in the state Deposit")
    restart(d, True)
    restart(d, False)
    trade_state(d, True, "Deposit")
    trade_state(d, False, "Deposit")
    d.note(N, "The test mines 17 blocks: the deposit has<br/>20 confirmations, the warning lock time")
    d.part("the buyer publishes his warning transaction")
    publish_warning(d)
    d.state(BD, "BuyersWarning")
    restart(d, True)
    trade_state(d, True, "BuyersWarning")
    d.note(BD, "musigd knows the decision after its start<br/>and sends the warning transaction")
    d.p2p(BD, "buyer's warning transaction")
    publish_warning(d, again=True)
    d.part("the warning is confirmed while the seller's musigd does not run")
    stop(d, False)
    warning_confirmed(d)
    d.answer(BD, B, "event: TX_CONFIRMED, WARNING_TX, ID of the warning transaction")
    d.note(N, "The test mines 1 block")
    d.part("the seller's musigd answers with the redirect")
    restart(d, False, running=False)
    wait_for_state(d, False, "BuyersWarning", "BUYERS_WARNING")
    d.note(SD, "musigd found the warning in the blocks that it missed.<br/>It sends its redirect transaction on its own,<br/>with a child transaction that raises the fee")
    d.p2p(SD, "seller's redirect transaction, child transaction")
    wait_for_state(d, False, "TradeClosed(SellersRedirect)", "TRADE_CLOSED, SELLERS_REDIRECT", mine=True)
    wait_for_state(d, True, "TradeClosed(SellersRedirect)", "TRADE_CLOSED, SELLERS_REDIRECT")
    d.part("the closed trade after a further restart")
    d.note(N, "The test mines 3 blocks")
    restart(d, True)
    restart(d, False)
    for buyer in (True, False):
        wait_for_state(
            d, buyer, "TradeClosed(SellersRedirect), final", "TRADE_CLOSED, SELLERS_REDIRECT, final", extra=", no deadlines"
        )
    d.part("balances")
    balance_until(d, True, "1 BTC - buyer's security deposit, less fees")
    balance_until(d, False, "1 BTC - trade amount - seller's security deposit,<br/>less the fee of the child transaction")
    return d


def test_7():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    publish_and_confirm(d)
    d.note(N, "The test mines 17 blocks: the deposit has<br/>20 confirmations, the warning lock time")
    d.part("the buyer publishes his warning transaction, the seller's musigd does not run")
    stop(d, False)
    publish_warning(d)
    d.p2p(BD, "buyer's warning transaction")
    d.state(BD, "BuyersWarning")
    warning_confirmed(d)
    d.answer(BD, B, "event: TX_CONFIRMED, WARNING_TX,<br/>one deadline: CLAIM_LOCK_TIME")
    d.part("restart of the buyer's musigd before the claim")
    restart(d, True)
    wait_for_state(d, True, "BuyersWarning", "BUYERS_WARNING with a deadline", extra=", the same deadline CLAIM_LOCK_TIME")
    d.loop("until the first event reports TRADE_CLOSED, BUYERS_CLAIM")
    d.note(N, "The test mines 1 block")
    d.opt("the claim lock time of 10 blocks has passed")
    d.note(BD, "musigd builds, signs and sends<br/>its claim transaction on its own")
    d.p2p(BD, "buyer's claim transaction")
    d.end()
    d.call(B, BD, "Trade.SubscribeTradeEvents(trade-1)")
    d.answer(BD, B, "first event: STATE,<br/>at the end with a height above the deadline")
    d.end()
    d.state(BD, "TradeClosed(BuyersClaim)", True)
    d.part("the seller's musigd starts after the claim")
    restart(d, False, running=False)
    wait_for_state(d, False, "TradeClosed(BuyersClaim)", "TRADE_CLOSED, BUYERS_CLAIM")
    d.part("balances")
    balance_until(d, True, "1 BTC + trade amount + seller's security deposit, less fees")
    balance_until(d, False, "1 BTC - trade amount - seller's security deposit, less fees")
    return d


def sign_custom(d, buyer, again=False):
    client, daemon, who = side(buyer)
    d.loop("until the request is accepted")
    d.call(client, daemon, "Trade.SignCustomPayoutTx(seller's amount 3,000,000 sat, fee rate)")
    d.end()
    d.answer(daemon, client, "the same PSBT as before" if again else f"custom payout PSBT with the {who}'s signatures, transaction ID")


def test_8():
    d = Diagram(TWO)
    funded_daemons(d)
    set_up_trade(d)
    publish_and_confirm(d)
    d.part("both traders sign the custom payout")
    sign_custom(d, True)
    d.state(BD, "CustomPayoutSigned")
    sign_custom(d, False)
    d.state(SD, "CustomPayoutSigned")
    d.note(TRADERS, "The test checks: both PSBTs have the same transaction ID")
    d.part("restart of both daemons")
    restart(d, True)
    restart(d, False)
    trade_state(d, True, "CustomPayoutSigned")
    trade_state(d, False, "CustomPayoutSigned")
    sign_custom(d, True, again=True)
    sign_custom(d, False, again=True)
    d.part("the seller finishes the custom payout, the buyer's musigd does not run")
    stop(d, True)
    d.message(B, S, "Buyer's custom payout PSBT")
    d.loop("until the request is accepted")
    d.call(S, SD, "Trade.CustomCloseTrade(buyer's custom payout PSBT)")
    d.end()
    d.answer(SD, S, "signed custom payout transaction")
    d.state(SD, "TradeClosed(Custom)")
    restart(d, False)
    d.note(SD, "musigd knows the decision after its start<br/>and sends the custom payout transaction")
    d.p2p(SD, "custom payout transaction")
    wait_for_state(d, False, "TradeClosed(Custom), final", "TRADE_CLOSED, CUSTOM, final", mine=True)
    d.part("the buyer's musigd starts after the payout")
    restart(d, True, running=False)
    d.note(BD, "musigd finds the custom payout transaction<br/>in the blocks that it missed")
    wait_for_state(d, True, "TradeClosed(Custom)", "TRADE_CLOSED, CUSTOM")
    d.part("balances")
    balance_until(d, True, "1 BTC + 3,200,000 sat, less fees")
    balance_until(d, False, "1 BTC - 3,200,000 sat, less fees")
    return d


def test_9():
    d = Diagram(TWO)
    funded_daemons(d, timeout=8)
    set_up_trade(d)
    d.note(TRADERS, "The test does not call Trade.PublishDepositTx:<br/>nobody publishes the deposit transaction")
    d.part("the deposit timeout passes while no musigd runs")
    trade_state(d, False, "Deposit", extra=",<br/>one deadline: DEPOSIT_TIMEOUT at the current height + 9")
    stop(d, True)
    stop(d, False)
    d.note(N, "The test mines 9 blocks")
    d.part("the seller's musigd aborts the deposit")
    restart(d, False, running=False)
    trade_state(d, False, "Deposit", checked=False, extra=", the same deadline DEPOSIT_TIMEOUT")
    d.note(SD, "The deposit timeout has passed:<br/>musigd sends its deposit abort<br/>transaction on its own")
    d.p2p(SD, "seller's deposit abort transaction")
    wait_for_state(d, False, "TradeClosed(DepositAborted)", "TRADE_CLOSED, DEPOSIT_ABORTED", mine=True)
    balance_until(d, False, "1 BTC, less the fee of the abort")
    d.part("the buyer's musigd starts after the abort")
    restart(d, True, running=False)
    d.note(BD, "musigd finds the seller's abort in the blocks<br/>that it missed. It sends no transaction")
    wait_for_state(d, True, "TradeClosed(DepositAborted)", "TRADE_CLOSED, DEPOSIT_ABORTED")
    balance_until(d, True, "1 BTC exactly")
    return d


def killed_request(d, buyer, request, delay, answer, loop_answer=True):
    client, daemon, _ = side(buyer)
    d.call(client, daemon, request)
    d.note(daemon, f"The test ends the process {delay} ms after the request (kill).<br/>The response arrives or not")
    restart(d, buyer, running=False)
    d.loop("until the request is accepted")
    d.call(client, daemon, request)
    d.end()
    d.answer(daemon, client, answer)


def test_10():
    d = Diagram(TWO)
    funded_daemons(d)
    d.part("a setup is interrupted, trade-0")
    d.call(B, BD, "Trade.InitTrade(trade-0, BUYER_AS_TAKER)")
    d.answer(BD, B, "buyer's public key shares, script key")
    d.note(BD, "State of trade-0: Init")
    d.call(S, SD, "Trade.InitTrade(trade-0, SELLER_AS_MAKER)")
    d.answer(SD, S, "seller's public key shares, script key")
    d.note(SD, "State of trade-0: Init")
    d.message(S, B, "Seller's public key shares, script key (part of message B)")
    d.call(B, BD, "Trade.GetNonceShares(seller's keys, trade parameters)")
    d.note(BD, "The test ends the process 1 ms after the request (kill),<br/>while musigd selects and reserves the coins")
    restart(d, True, running=False)
    d.call(B, BD, "Trade.SubscribeTradeEvents(trade-0)")
    d.answer(BD, B, "first event: STATE")
    d.note(BD, "State of trade-0: TradeClosed(Abandoned) (checked)")
    balance_until(d, True, "1 BTC exactly (the coins are free again)")
    d.call(B, BD, "Trade.InitTrade(trade-0, BUYER_AS_TAKER)")
    d.refused(BD, B, "ALREADY_EXISTS: the trade ID is used")
    d.note(SD, "The seller's trade-0 stays in the state Init.<br/>The test does not use it again")
    set_up_trade(d)
    publish_and_confirm(d)
    d.part("message E, with a kill of the buyer's musigd")
    killed_request(
        d, True, "Trade.GetPartialSignatures(buyerReadyToRelease = true)", 25,
        "buyer's partial signature for the swap transaction,<br/>equal to the first response if that one arrived",
    )
    d.state(BD, "BuyerReadyToRelease")
    d.message(B, S, "Message E: buyer's partial signature for the swap transaction")
    d.part("message F, with two kills of the seller's musigd")
    killed_request(
        d, False, "Trade.SignSwapTx(buyer's partial signature,<br/>sellerReadyToRelease = false)", 4,
        "response without a key share",
    )
    d.state(SD, "BuyerReadyToRelease")
    killed_request(
        d, False, "Trade.SignSwapTx(no signature, sellerReadyToRelease = true)", 60,
        "seller's secret key share for the buyer's payout,<br/>equal to the first response if that one arrived",
    )
    d.state(SD, "SellerReadyToRelease")
    d.message(S, B, "Message F: seller's secret key share for the buyer's payout")
    d.part("message G, with a kill of each musigd")
    killed_request(
        d, True, "Trade.CloseTrade(seller's secret key share)", 10,
        "buyer's secret key share for the seller's payout,<br/>equal to the first response if that one arrived",
    )
    d.state(BD, "TradeClosed(Cooperative)")
    d.message(B, S, "Message G: buyer's secret key share for the seller's payout")
    killed_request(
        d, False, "Trade.CloseTrade(buyer's secret key share)", 2,
        "response without a key share",
    )
    d.state(SD, "TradeClosed(Cooperative)")
    d.part("result")
    trade_state(d, True, "TradeClosed(Cooperative)")
    trade_state(d, False, "TradeClosed(Cooperative)")
    balance_until(d, True, "1 BTC + trade amount, less fees")
    balance_until(d, False, "1 BTC - trade amount, less fees")
    return d


def musigd_without_peer(d):
    d.note("D", "The test starts musigd: new wallet directory, no peer")


def test_11():
    d = Diagram([("C", "bisq2 (test)"), ("D", "musigd"), ("T", "Test")])
    musigd_without_peer(d)
    d.call("C", "D", "Wallet.OpenOrCreateWallet(password)")
    d.note("D", "The request runs: musigd derives the key of the<br/>wallet database from the password")
    d.message("T", "D", "signal SIGTERM, 200 ms after the request")
    d.note("D", "musigd accepts no new request and<br/>completes the running request")
    d.answer("D", "C", "ok: the wallet is created")
    d.answer("D", "T", "the process ends with the exit status 0,<br/>less than 10 s after the signal")
    d.call("C", "D", "new connection to the port")
    d.refused("D", "C", "no connection: the port is closed")
    d.note("D", "The test starts musigd again<br/>on the same wallet directory")
    d.call("C", "D", "Wallet.IsWalletReady()")
    d.answer("D", "C", "exists = true")
    return d


def test_12():
    d = Diagram([("C", "bisq2 (test)"), ("D", "musigd"), ("T", "Test")])
    musigd_without_peer(d)
    d.call("C", "D", "Wallet.SubscribeWalletEvents()")
    d.note("D", "The stream is open. No wallet is open,<br/>so it has no event")
    d.message("T", "D", "signal SIGTERM")
    d.answer("D", "C", "the stream ends without an error")
    d.answer("D", "T", "the process ends with the exit status 0,<br/>less than 10 s after the signal")
    return d


def test_13():
    d = Diagram([("C", "bisq2 (test)"), ("D", "musigd"), ("T", "Test")])
    musigd_without_peer(d)
    d.call("C", "D", "Wallet.IsWalletReady: the request is started,<br/>its message is never sent")
    d.note("D", "The request is open: musigd waits for its message")
    d.message("T", "D", "signal SIGTERM, 500 ms after the start of the request")
    d.note("D", "musigd waits for the open request,<br/>up to the limit of 10 s")
    d.refused("D", "C", "the request ends without a response")
    d.answer("D", "T", "the process ends with the exit status 0,<br/>between 10 s and 20 s after the signal")
    return d


TESTS = [
    (
        "musigd_serves_trade_service_and_wallet_service",
        "One `musigd` without a peer serves both services. The trade service refuses to create a "
        "trade while no wallet is open, and creates it after the wallet was created.",
        test_1,
    ),
    (
        "musigd_does_not_start_with_a_lock_time_outside_the_limits",
        "`musigd` reads the lock times from the environment. With a value outside the limits, or "
        "with a value that is no number, the process reports the value and ends before it serves "
        "a request. The test makes no API call, so the diagram shows only the three starts.",
        test_2,
    ),
    (
        "j11_trade_of_two_daemons_without_bitcoin_core_rpc",
        "The complete trade without a problem (test IDs J11, A1, B1, M1): setup, deposit "
        "transaction over the peer-to-peer network, confirmations reported to both traders, "
        "cooperative close with the messages E, F and G, payouts in both wallets, and the drain "
        "of the buyer's payout at `Wallet.Close`.",
        test_3,
    ),
    (
        "c4_forced_close_of_two_daemons",
        "The seller releases and the buyer does not answer (test IDs C1, C2, C4). The seller's "
        "`musigd` publishes the swap transaction on its own before the buyer's warning "
        "transaction becomes valid. The buyer's `musigd` reads the seller's key share from the "
        "confirmed swap transaction and closes the trade. No request asks for either step.",
        test_4,
    ),
    (
        "h2_h5_restart_of_two_daemons_during_a_forced_close",
        "As the test before, with restarts (test IDs H2, H3, H5, K2). Both daemons are ended and "
        "started after the release, and report the states in which they were ended. The buyer's "
        "`musigd` does not run while the swap transaction is confirmed, and closes the trade "
        "after its start.",
        test_5,
    ),
    (
        "h2_h5_restart_of_two_daemons_in_the_dispute_path",
        "The dispute path with a redirect, with restarts (test IDs H2, H3, H5). The buyer "
        "publishes his warning transaction and his `musigd` is ended directly afterwards. The "
        "seller's `musigd` does not run while the warning is confirmed, and answers with the "
        "redirect transaction after its start.",
        test_6,
    ),
    (
        "h2_h5_m6_restart_of_two_daemons_before_a_claim",
        "The dispute path with a claim (test IDs H2, H5, M6). The seller's `musigd` does not run. "
        "The buyer's `musigd` is restarted while it waits for the claim lock time, keeps the same "
        "deadline, and claims the escrow when the lock time has passed.",
        test_7,
    ),
    (
        "h2_h5_restart_of_two_daemons_during_a_custom_payout",
        "The custom payout, with restarts (test IDs H2, H3, H5). Both traders sign a payout of "
        "3,000,000 sat to the seller. The seller finishes the transaction while the buyer's "
        "`musigd` does not run. The buyer never finishes it himself and finds it on the chain.",
        test_8,
    ),
    (
        "a3_h2_deposit_abort_of_two_daemons_after_a_restart",
        "The deposit abort (test IDs A3, H2, H5). Both traders sign the deposit transaction and "
        "nobody publishes it. Both daemons have a deposit timeout of 8 blocks and do not run "
        "while it passes. The seller's `musigd` aborts after its start. The buyer's `musigd` "
        "finds the abort of the seller.",
        test_9,
    ),
    (
        "h3_h4_h8_daemon_is_ended_while_it_processes_a_request",
        "A `musigd` is ended while it processes a request (test IDs H3, H4, H8): once during the "
        "setup of the trade `trade-0`, and once for every request of the cooperative close of "
        "the trade `trade-1`. An interrupted setup is abandoned. Every later request is repeated "
        "after the start and is accepted.",
        test_10,
    ),
    (
        "musigd_shuts_down_on_sigterm_and_completes_the_running_request",
        "The regular shutdown (`docs/specifications/trade/shutdown.md`). The signal SIGTERM "
        "arrives while `musigd` creates a wallet. The process completes the request, accepts no "
        "new connection, and ends with the exit status 0. The created wallet is stored.",
        test_11,
    ),
    (
        "musigd_ends_an_open_subscription_when_it_shuts_down",
        "The regular shutdown with a client that keeps a subscription open. The process ends "
        "the stream of the subscription at the signal and does not wait for the client.",
        test_12,
    ),
    (
        "musigd_shuts_down_at_the_limit_while_a_request_is_open",
        "The regular shutdown with a client that does not complete a request. The process waits "
        "for the request up to the limit of 10 seconds, then ends it, and still ends with the "
        "exit status 0.",
        test_13,
    ),
]

HEADER = """# Sequence diagrams of the tests in `rpc/tests/test_musigd.rs`

This file shows what every test of `rpc/tests/test_musigd.rs` does, as one UML sequence diagram
per test. The diagrams follow the test code of 2026-11-20.

This file is generated. Do not edit it by hand: when a test changes, change its description in
`rpc/test-musigd-sequence-diagrams.py` and run that script again.

The tests start real `musigd` processes and use only their gRPC interface. In the tests of a
whole trade, the test plays the role of bisq2 for both traders: it calls the services of each
trader's `musigd` and carries the messages of the trade protocol from one trader to the other.

## How to view the diagrams

The diagrams are written in [Mermaid](https://mermaid.js.org) (open source, MIT licence), inside
this Markdown file. Mermaid was chosen because a reader needs no extra program in most cases:

| Where | What to do |
|---|---|
| GitHub | Nothing. GitHub draws Mermaid diagrams in Markdown files. |
| IntelliJ IDEA and other JetBrains IDEs | Install the plugin "Mermaid" (by JetBrains). The Markdown preview then draws the diagrams. |
| Visual Studio Code | Open the Markdown preview. If it shows the text of a diagram instead of a drawing, install the extension "Markdown Preview Mermaid Support". |
| Any browser | Copy the text of one diagram into the Mermaid Live Editor, <https://mermaid.live>. It can also save the diagram as SVG or PNG. |
| Command line | `npx -p @mermaid-js/mermaid-cli mmdc -i rpc/test-musigd-sequence-diagrams.md -o diagrams.svg` writes one SVG file per diagram (`diagrams-1.svg`, `diagrams-2.svg`, ...). |

The alternative PlantUML has more UML elements, but it needs Java and a plugin or a server to
draw a diagram, and GitHub does not draw it.

## How to read the diagrams

Lifelines:

| Lifeline | Meaning |
|---|---|
| Buyer's bisq2 (test), Seller's bisq2 (test) | The test, in the role of the bisq2 application of this trader |
| Buyer's musigd, Seller's musigd | The `musigd` process of this trader, with its wallet |
| bitcoind (regtest) | The Bitcoin node of the test environment. A `musigd` reaches it only over the peer-to-peer network, with compact block filters. |

Arrows:

| Arrow | Meaning |
|---|---|
| bisq2 to musigd, solid | A gRPC request. `Trade.` is the trade service `musigrpc.Musig`, `Wallet.` is the wallet service `wallet.Wallet`. |
| musigd to bisq2, dashed | The response, or an event of a stream |
| musigd to bisq2, dashed with a cross | The request is refused with this gRPC status |
| bisq2 to bisq2, solid | A message between the traders (A to G, see `rpc/musig-trade-protocol-messages.txt`). In the test no network carries it: the test passes the data of one response into the next request. |
| bisq2 to bisq2, solid with a cross | A message that the test does not deliver |
| musigd to bitcoind, "P2P" | A transaction that `musigd` sends to the node over the peer-to-peer network |

Notes:

- "State: ..." is the state of the trade of this trader after the step. "(checked)" means that the
  test reads this state from the service and asserts it. A state without this mark follows from
  `docs/specifications/trade/trade-states.md` and the engine code; the test does not read it at
  this point.
- "Part: ..." starts a part of the test.
- A note on bitcoind describes what the test does with the node through its RPC interface: it
  mines blocks and pays coins. No `musigd` uses this interface.
- A note on a `musigd` describes a start or an end of the process, or something that `musigd` does
  on its own, without a request.

Every stream request (`PublishDepositTx`, `SubscribeTxConfirmationStatus`,
`SubscribeTradeEvents`) is drawn once. The events that the test reads from the stream follow as
dashed arrows. A request that the test repeats until `musigd` accepts it is drawn as a loop:
directly after a start, `musigd` refuses a request that needs the chain until it has evaluated
the blocks that it missed.

Values that all tests of a trade use:

| Value | Amount |
|---|---|
| Funding of each wallet | 100,000,000 sat (1 BTC) |
| Trade amount | 5,000,000 sat |
| Buyer's security deposit | 1,000,000 sat |
| Seller's security deposit | 1,200,000 sat |
| Deposit fee rate and prepared fee rate | 2,500 sat per 1000 weight units (10 sat per vbyte) |
| Warning lock time, claim lock time | 20 blocks, 10 blocks |
| Trade ID | `trade-1` |

The buyer is the taker and the seller is the maker in every trade.

## Tests

| No. | Test | Diagram shows |
|---|---|---|
"""

SUMMARY = [
    "Both services of one `musigd`; no trade without an open wallet",
    "Start of `musigd` with invalid lock times",
    "Whole trade with a cooperative close and a drain",
    "Forced close: swap transaction of the seller's `musigd`",
    "Forced close with restarts",
    "Warning and redirect, with restarts",
    "Warning and claim, with a restart",
    "Custom payout, with restarts",
    "Deposit abort after the deposit timeout",
    "End of `musigd` during a request",
    "Regular shutdown during a request",
    "Regular shutdown with an open subscription",
    "Regular shutdown with a request that is never completed",
]


def main(path):
    out = [HEADER.rstrip("\n")]
    for number, ((name, _, _), summary) in enumerate(zip(TESTS, SUMMARY), 1):
        out.append(f"| {number} | [`{name}`](#{number}-{name}) | {summary} |")
    out.append("")
    for number, (name, description, build) in enumerate(TESTS, 1):
        out.append(f"## {number}. `{name}`")
        out.append("")
        out.append(textwrap.fill(description, 100, break_long_words=False, break_on_hyphens=False))
        out.append("")
        out.append(build().render())
    text = "\n".join(out)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else pathlib.Path(__file__).with_suffix(".md"))
