# Service interface

This file defines the meaning of the requests that bisq2 sends to the trade service of `musigd`,
their preconditions, and the rules for errors and repeated requests. It does not define the
layout of the protobuf messages.

## Parties

- **Caller**: the bisq2 application of the user. It runs on the same computer as `musigd`.
- **Service**: the gRPC service `musigrpc.Musig` of `musigd`. It gives the caller access to the
  trade engine.
- **Peer**: the other trader. The peer never talks to the service. The caller passes the peer's
  messages on as data of its requests.

The caller decides when a step of the trade happens: when the payment is started, when it is
confirmed, whether a mediation result is accepted, whether a dispute is started. The engine
decides whether the step is allowed, performs all cryptographic operations, holds all keys, and
performs all reactions to chain events on its own.

## Security assumptions

- The service has no authentication and no encryption.
- The service listens only on the loopback address. No option of `musigd` changes this.
- The computer and all its local users and processes are trusted. Every local process can send
  every request: close a trade, sign a swap transaction, or use the wallet service while the
  wallet is open.
- Reading the seed words needs the wallet password.

This section adopts a proposal that was not yet confirmed. It is open point O5 in
[README.md](README.md).

## Requests

The table lists the requests with their existing names. "State" is the state of the trader's own
trade before the request.

| Request | Role | State | Meaning | Defined in |
|---|---|---|---|---|
| `InitTrade` | Both | No trade with this ID | Creates the trade. Returns the own public key shares and script key. | [setup-and-deposit.md](setup-and-deposit.md), step 1 |
| `GetNonceShares` | Both | `Init` | Takes the peer's key shares and the trade parameters. Returns the own half of the deposit, addresses and nonces, and the amount to distribute to the redirection receivers. | Step 2 |
| `GetPartialSignatures` | Both | `Init` | Takes the peer's half of the deposit, addresses and nonces, and the redirection receivers. Returns the own partial signatures and the transaction IDs. | Step 3 |
| `SignDepositTx` | Both | `Init` | Takes the peer's partial signatures. Signs the own deposit inputs. | Step 4 |
| `PublishDepositTx` | Both | `Deposit` | Takes the deposit with the peer's signatures. Publishes it. Returns the events of the deposit. | Step 5 |
| `SubscribeTxConfirmationStatus` | Both | `Deposit` or later | Returns the events of the deposit for the trader who did not publish it. | [chain-observation.md](chain-observation.md) |
| `GetPartialSignatures` with the payment start flag | Buyer | `Deposit` | Starts the payment. Returns message E. | [closing.md](closing.md) |
| `SignSwapTx` with a partial signature | Seller | `Deposit` | Takes message E. | [closing.md](closing.md) |
| `SignSwapTx` with the release flag | Seller | `BuyerReadyToRelease` | Releases. Returns message F. | [closing.md](closing.md) |
| `CloseTrade` with a key share | Buyer | `BuyerReadyToRelease` | Takes message F. Returns message G. | [closing.md](closing.md) |
| `CloseTrade` with a key share | Seller | `SellerReadyToRelease` | Takes message G. | [closing.md](closing.md) |
| `CloseTrade` without data | Seller | `SellerReadyToRelease` | Publishes the swap transaction. | [closing.md](closing.md) |
| `SignCustomPayoutTx` | Both | `Deposit`, `BuyerReadyToRelease` | Signs a custom payout. | [closing.md](closing.md) |
| `CustomCloseTrade` | Both | `CustomPayoutSigned` | Takes the peer's custom payout PSBT. Finalises and publishes. | [closing.md](closing.md) |
| `AbandonTrade` | Both | `Init` | Ends the trade and releases the reserved coins. Refused when the trader has signed the deposit transaction. | [setup-and-deposit.md](setup-and-deposit.md) |
| `PublishWarningTx` | Both | `Deposit`, `BuyerReadyToRelease`, `CustomPayoutSigned` | Starts the dispute path: the trader decides to publish his warning transaction. Returns the transaction. | [dispute.md](dispute.md) |
| `SubscribeTradeEvents` | Both | Any | Returns the events of the trade. The first event is the current state. | Below, "Events of a trade" |

`CloseTrade` with a swap transaction, by which the buyer passes the swap transaction to his
engine, exists for tests. In operation the buyer's engine finds the transaction on the chain.

Requests that the interface needs in addition:

| Request | Role | Meaning | Defined in |
|---|---|---|---|
| Close of the wallet service (exists as `Close` of `wallet.Wallet`) | - | Drains the imported coins when the user ends the program | [post-trade-protection.md](post-trade-protection.md) |

### Events of a trade

`SubscribeTradeEvents` returns one stream per trade. Every event contains the state of the trade
after the event, the closure type of a closed trade, whether the trade is final, and the height
of the chain as the engine sees it.

| Event | Meaning |
|---|---|
| State | The first event of a stream, and every change of the state. After a reorganisation the new state can be an earlier state of the trade. |
| Transaction sent | The engine sent a transaction of the trade to the network: its kind and its ID, and the ID of the child if the transaction was sent together with a child that raises its fee ([fees.md](fees.md)). This does not mean that a node accepted it. |
| Loss | The peer took funds of the trader with a claim or a penalty transaction. |
| Unexpected | Something happened on the chain that the engine does not expect, or the engine could not do what the trade needs, for example raise the fee of a transaction because the wallet holds no coin for it. No state changed. |
| Transaction confirmed | The number of confirmations of a transaction of the trade changed: its kind, its ID and the number. It is reported from the first confirmation until the transaction counts as confirmed (3 confirmations), and with the number 0 when a reorganisation removed the transaction. Blocks that arrive together can give one event. |
| Status | A deadline, a required action, the terminated mark or the final mark changed, and the state did not. |
| Problem with the peers, and its end | While the problem lasts, the chain is not observed and nothing can be sent ([chain-observation.md](chain-observation.md)). |

Every event also contains, as they are after the event:

- the reason of the termination, if the trade is terminated ([trade-states.md](trade-states.md));
- the deadlines that are in force;
- the actions that the user must take.

A deadline is a block height: the height of the last block of the chain at which a rule of the
engine changes. It follows from the same data as the rule itself.

| Deadline | Reported while | Meaning of the height |
|---|---|---|
| Deposit timeout | The deposit is unconfirmed, state `Deposit` | The engine sends the deposit abort transaction |
| Warning lock time | The deposit is confirmed and both payouts are unspent, also after a cooperative close | From this height on a warning transaction is valid: the request to publish the warning is accepted, and a warning of either trader can be in the next block |
| Last release | Seller, state `Deposit` or `BuyerReadyToRelease`, deposit confirmed | The last height at which the seller's release is accepted ([closing.md](closing.md), release safety margin) |
| Forced close | Seller, state `SellerReadyToRelease` | The seller's engine publishes the swap transaction on its own ([closing.md](closing.md)) |
| Claim lock time | A warning transaction is confirmed and its escrow is unspent | From this height on the claim of the trader who published the warning is valid, and his engine sends it. The redirect or the penalty of the other trader must be confirmed at this height at the latest. |

An action is something that the engine cannot do on its own:

| Action | Reported while | What the user must do |
|---|---|---|
| Publish the warning | The trade is terminated, the deposit is confirmed, both payouts are unspent, and the state allows the warning | No step with the peer is possible any more. The trader must publish his warning transaction when the warning lock time has passed, unless the peer publishes his own first. |
| Start the dispute | The trade is not terminated, the state is `Deposit` or `BuyerReadyToRelease`, and the last release has passed | The trade cannot end with a release any more. The traders must agree on a custom payout, or a trader must publish his warning transaction when the warning lock time has passed. |
| Spend or drain the payout | The trade is closed cooperatively and both payouts are unspent | The peer can still publish his warning, and the engine answers only while the program runs. The user must spend his payout, or end the program in the regular way ([post-trade-protection.md](post-trade-protection.md)). |

The set of deadlines and actions, and the form of the events, are not agreed with bisq2. This is
open point O18 in [README.md](README.md).

Changes of existing messages that this specification requires:

- The lock times are not part of any message. They are configuration values of the engine
  ([parameters.md](parameters.md)).
- The trade parameters stay in the request of step 2 (`GetNonceShares`), as in the existing
  messages. This is open point O13 in [README.md](README.md).
- The nonces and the partial signature of the claim transactions are removed
  ([transactions.md](transactions.md)). The payout address of each trader stays.
- The block height in responses is the height of the chain as the engine sees it.

## Wallet of the trade service

The trade service uses the wallet of the wallet service of the same `musigd` process. It has no
wallet of its own.

- Every request of the trade service needs the open wallet. While no wallet is open, a request is
  refused. The trade is not changed, and the request can be sent again when the wallet is open.
- The engine uses the wallet for the key shares, the keys and addresses of a trade, the coins of
  the deposit and their reservation, the signatures of the wallet, the record of the deposit
  transaction, and the import of the payout key
  ([../wallet/README.md](../wallet/README.md)).
- A `musigd` process that was started without a wallet directory cannot start a trade.

Rationale: the deposit is paid from the coins of the user, and every key and address of a trade
comes from the wallet of the user. A second wallet for trades, or test data in place of a wallet,
would allow a trade that the user cannot fund or cannot spend from.

## Processing of a request

The order of checks, the handling of repeated requests and the terminated mark are defined in
[trade-states.md](trade-states.md). In short:

- A request for an unknown trade ID is refused.
- `InitTrade` for a known trade ID is refused. The existing trade is not changed.
- A request that is not allowed for the role, the state or the chain conditions is refused. The
  trade is not changed and continues. The request can be repeated.
- A request with invalid data that the caller itself supplied is refused in the same way.
- A request with data of the peer that an unmodified client never produces terminates the trade.
  The engine logs the reason in detail.
- A request that is repeated with the same data returns the same response.

## Errors

Every error tells the caller which of these cases applies:

| Case | Meaning for the caller |
|---|---|
| Refused | A precondition does not hold, or data of the caller is invalid. The trade continues. The request can be sent again. |
| Terminated | The peer sent data that an unmodified client never produces, now or earlier. The trade is terminated. The error contains the reason. Sending the request again is useless. |
| Unknown trade, known trade ID | The trade ID is wrong for this request. |
| Temporary | The engine could not process the request for a reason outside of the trade, for example the wallet is closed or no peer is reachable. The request can be sent again. |
| Internal | A defect of the engine. The state of the trade is as before the request. |

A request never leaves the caller without a statement about the state of the trade: after every
error the state is the state before the request, except that the trade can be terminated.

The assignment of these cases to gRPC status codes and error details is open point O11 in
[README.md](README.md). Until it is decided, the service uses these status codes, and the message
of the status describes the reason:

| Case | Status code |
|---|---|
| Refused: a precondition does not hold (role, state, order) | `FAILED_PRECONDITION` |
| Refused: a condition of the chain does not hold yet (confirmations of the deposit, warning lock time, release safety margin, a spent payout) | `FAILED_PRECONDITION`. The message names the condition. The caller can repeat the request later. |
| Refused: data of the caller is invalid (trade parameters, redirection receivers, a field that cannot be decoded) | `INVALID_ARGUMENT`; `OUT_OF_RANGE` for a number above its range; `NOT_FOUND` for a missing part of a request |
| Terminated | `ABORTED`. The message starts with "trade was terminated" and contains the reason. |
| Unknown trade | `NOT_FOUND` |
| Known trade ID in `InitTrade`, also the ID of a closed or abandoned trade, and also after a restart | `ALREADY_EXISTS` |
| The trades of the wallet could not be loaded ([persistence-and-recovery.md](persistence-and-recovery.md)); every request for a trade | `DATA_LOSS`. The message names the trade and the reason. |
| The trade could not be stored | `INTERNAL`. Nothing that the request produced is handed out. The caller can repeat the request. |
| Temporary: no wallet is open | `FAILED_PRECONDITION` |
| Temporary: `musigd` was started without a peer, so the engine has no access to the chain, and the request needs it | `FAILED_PRECONDITION` |
| Temporary: no peer is reachable, or no peer serves compact block filters, and the request must send a transaction (`PublishDepositTx`, `PublishWarningTx`) | `UNAVAILABLE`. The message names the problem. The trade is not changed, and the caller can repeat the request. |
| Internal | `INTERNAL` |

## Events and responses

- A response or an event that says "sent" for a transaction does not mean that a node accepted it
  ([chain-observation.md](chain-observation.md)).
- The deposit is reported as confirmed at 3 confirmations.
- Deadlines are reported as block heights.

## Logging

The service logs requests and responses. Secret key shares, partial signatures of the swap
transaction, the seed and passwords are replaced by a placeholder in the log, at every log level.
