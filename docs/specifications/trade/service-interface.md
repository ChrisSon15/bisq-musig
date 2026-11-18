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

`CloseTrade` with a swap transaction, by which the buyer passes the swap transaction to his
engine, exists for tests. In operation the buyer's engine finds the transaction on the chain.

Requests that the interface needs in addition:

| Request | Role | Meaning | Defined in |
|---|---|---|---|
| Publish the warning transaction | Both | Starts the dispute path | [dispute.md](dispute.md) |
| Events of a trade | Both | One stream per trade: state changes, the terminated mark, transactions sent and confirmed, deadlines as block heights, actions that the user must take, losses | [chain-observation.md](chain-observation.md) |
| Close of the wallet service (exists as `Close` of `wallet.Wallet`) | - | Drains the imported coins when the user ends the program | [post-trade-protection.md](post-trade-protection.md) |

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
| Refused: data of the caller is invalid (trade parameters, redirection receivers, a field that cannot be decoded) | `INVALID_ARGUMENT`; `OUT_OF_RANGE` for a number above its range; `NOT_FOUND` for a missing part of a request |
| Terminated | `ABORTED`. The message starts with "trade was terminated" and contains the reason. |
| Unknown trade | `NOT_FOUND` |
| Known trade ID in `InitTrade` | `ALREADY_EXISTS` |
| Temporary: no wallet is open | `FAILED_PRECONDITION` |
| Internal | `INTERNAL` |

## Events and responses

- A response or an event that says "sent" for a transaction does not mean that a node accepted it
  ([chain-observation.md](chain-observation.md)).
- The deposit is reported as confirmed at 3 confirmations.
- Deadlines are reported as block heights.

## Logging

The service logs requests and responses. Secret key shares, partial signatures of the swap
transaction, the seed and passwords are replaced by a placeholder in the log, at every log level.
