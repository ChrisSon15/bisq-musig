# Events of the wallet

This file defines the events by which the wallet service tells its caller that the wallet
changed. Without them the caller must ask for the balance and the transactions at intervals.

## Rules

1. **An event is a hint.** The state of the wallet is what the other requests of the wallet
   service return. A caller that missed an event loses nothing: it reads the state again.
2. **One stream per subscriber.** The request `SubscribeWalletEvents` returns the events from the
   moment of the request on. It is accepted while no wallet is open.
3. **Every stream gets the state first.** If a wallet is open, the first event is the state of
   the wallet. Otherwise the state follows as the first event when the wallet is opened.
4. **A subscriber that is too slow gets the state again.** The service keeps a limited number of
   events for a subscriber. When events were lost, the next event of the stream is the state.
5. **Events follow every change of the wallet,** whatever caused it: a sync with the chain, a
   payment, the drain at the close, the release of a reserved coin, and a request of a trade
   (reserved coins, the imported key of a payout, a recorded or forgotten transaction).

## Events

Every event contains the height of the last block that the wallet has seen, after the change.

| Event | Content | Meaning |
|---|---|---|
| State | Balance | The first event, the event when the wallet is opened, and the event after lost events. The caller reads the transactions and the outputs again. |
| Chain tip | - | The wallet has seen a new last block. |
| Transaction | Transaction ID; height of its block, if it is in a block | A transaction of the wallet is new, or the block that contains it changed. After a reorganisation the block can change, and the transaction can be unconfirmed again. |
| Transaction removed | Transaction ID | The transaction is no longer valid for the wallet: another transaction spent its coins, or the wallet was told to forget it ([coin-reservation.md](coin-reservation.md)). |
| Balance | Balance | The balance changed. It has the fields of the balance request: the part that a new transaction can spend, and the reserved part. |

The events of one change come in this order: chain tip, transactions in the order of their IDs,
balance.

The caller computes the number of confirmations of a transaction from the height of its block
and the height of the last block. The service sends no event for a transaction only because a
new block raised this number.

## Transport

The events are a stream of the gRPC service of the wallet, like the events of a trade. An
earlier design note proposed a publish and subscribe socket (ZeroMQ) for them. Whether such a
second transport is needed is not decided.
