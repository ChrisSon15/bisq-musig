# Chain observation and broadcast

This file defines how the engine sees the chain, how it sends transactions, and how it reacts to
every chain event of a trade.

## Access to the chain

- The engine observes the chain only through compact block filters (BIP157, BIP158) and blocks
  that it loads from peers of the Bitcoin peer-to-peer network.
- The engine sends transactions only to peers of that network.
- The engine does not use the RPC interface of a Bitcoin node, an Electrum server or a block
  explorer.
- The wallet uses the same access to the chain as the engine: one connection to the peers serves
  both. Every rule of this section holds for the wallet as well.

Rationale: every server that answers questions about addresses or transactions learns which
trades belong to the user. A compact block filter client asks for filters of all blocks and
loads whole blocks, so a peer learns much less.

Consequences that the rest of this specification depends on:

- **The engine sees confirmed transactions only.** It has no view of the mempool of any node.
  Every rule that waits for a transaction waits for its confirmation.
- **A broadcast has no result.** The engine knows that it sent a transaction to a peer. It does
  not know whether any node accepted it.
- **Time is counted in blocks.** All timeouts and deadlines of the engine are block counts.

## Peers

The engine needs at least one reachable peer that serves compact block filters. The peers are a
start option of `musigd`. The network of the peers must be the network of the wallet.

- The engine connects only to these peers. It does not look for other peers, also not when none
  of these peers is reachable.
- While no peer is reachable, the engine keeps trying to connect, with pauses that grow up to
  half a minute. It reports this condition. No step that needs the chain proceeds, and no state
  changes.
- If no connected peer serves filters, the engine reports a clear error and behaves in the same
  way.
- A condition counts as a problem when it has lasted for some seconds. The engine has no peer for
  a moment at every start and at every new connection.
- While a problem lasts, a request of the caller that must send a transaction is refused, and the
  caller can repeat it ([service-interface.md](service-interface.md)). A request that only
  observes the chain is accepted: its events arrive when a peer is reachable again. A sync of the
  wallet ends with an error that names the problem, and the wallet keeps what it knows.
- The caller is told about a problem with the peers, and about its end, as an event of the trade
  ([service-interface.md](service-interface.md), "Events of a trade"). An accepted request that
  gives no events does not tell the caller whether the chain is observed.
- What the engine knows about the chain stays valid while it has no connection. After a new
  connection the engine evaluates the blocks that it missed. It notices a reorganisation that
  happened in between and treats it like one that it saw while it was connected.
- A peer of another network does not answer. The engine cannot tell it from a peer that is not
  reachable.

The first rule is a decision of the project owner of 2026-11-18 (D20 in
`concept/CompletionPlan.md`). The reason given for it: the software operates only on regtest at
present, where no other peers exist. The rule must be looked at again before the software is used
on another network.

A further reason for the rule, which holds on every network: the user chooses the peers, for
example his own node. A peer learns the IP address of the user and the transactions that the user
sends, and a client with a single peer depends on this peer for a complete view of the chain.
Peers that the engine finds itself would replace the user's choice.

A peer sees the IP address of the user and the transactions that the user sends. Hiding the IP
address is not part of this specification. This is open point O4 in [README.md](README.md).

## What the engine watches

For every trade that is not final, from the moment the trader signed the deposit:

| Watched | Purpose |
|---|---|
| The deposit transaction | Confirmation depth; deposit timeout |
| The inputs of the deposit transaction, of both traders | A confirmed transaction that spends one of them and is not the deposit: the deposit abort of a trader, or another spend. The deposit can then never confirm. |
| The buyer's payout and the seller's payout | Any spend: swap, custom payout, warning, a payout that the owner spends |
| The escrow output of the buyer's warning and of the seller's warning | Redirect, claim, penalty |
| Every transaction that the engine sent | Confirmation; repeated sending |

Watching works by output script: the engine tests the filter of every new block for the watched
scripts, loads a block that matches, and looks for the transactions of the trade in it.

A script or an output can be watched from an earlier block on. The engine then evaluates the
blocks since that block again. Until this is done, it does not count as synced: a transaction that
it does not know can still be in one of these blocks. The same holds for the scripts of the
wallet, for example for the scripts that the wallet derives after a payment, and for a key that is
imported with a start height.

## Confirmations

| Rule | Depth |
|---|---|
| The deposit counts as confirmed | 3 (`MIN_CONFIRMATIONS`) |
| The engine reacts to a transaction of the peer (redirect, penalty, reading the key from a spend of the seller's payout) | 1 |
| A trade is final and the engine stops watching it | Its closing transaction has 3 confirmations |

Rationale: a reaction to the peer has a deadline, the end of the claim lock time. Waiting for more
confirmations would only cost time. A reaction that turns out to be unnecessary after a
reorganisation does no harm, because its transaction is then invalid.

A trade that was closed cooperatively has no closing transaction. It is watched as defined in
[post-trade-protection.md](post-trade-protection.md).

## Sending transactions

Before the engine sends a transaction, it checks everything it can check itself: the lock time has
passed, and the inputs are unspent as far as it knows.

A transaction that was sent has the status "sent" until the engine sees it in a block. The engine
never reports a sent transaction as accepted or confirmed.

The engine sends a transaction again after every new block in which the transaction is not
confirmed, until:

- the transaction is confirmed, or
- another transaction that spends one of the same outputs is confirmed.

A sending that fails, for example because no peer is reachable, is repeated after a pause of some
seconds, without waiting for a new block.

A peer asks for a transaction that it is told about only if it does not have the transaction
already, and if it has not rejected the transaction since the last block. A sending for which no
peer asks is therefore no error. The engine waits for the request of a peer for a limited time
only.

While a transaction stays unconfirmed, the engine can raise its fee ([fees.md](fees.md)).

This section adopts a proposal that was not yet confirmed. It is open point O3 in
[README.md](README.md).

## Reorganisations

The engine follows the chain with the most work. When blocks are replaced, it evaluates all
watched outputs again for the new blocks.

| Event removed by the reorganisation | Reaction |
|---|---|
| Deposit depth falls below 3, the buyer has not started the payment and no custom payout is signed | The deposit counts as unconfirmed again. Requests that need a confirmed deposit are refused until the depth is 3 again. |
| The deposit is removed and the deposit timeout passes | Deposit abort ([setup-and-deposit.md](setup-and-deposit.md)) |
| The deposit is removed and a transaction that spends one of its inputs confirms | The trade is closed as `DepositAborted`. The trader's coins are free again. |
| A confirmed warning transaction | The counting of the redirect and claim lock times starts again when the warning confirms again. No redirect or claim is sent before. |
| A confirmed transaction that the engine sent | The engine sends it again. |
| The closing transaction of a trade, before it had 3 confirmations | The trade is not final. The engine continues with the state before the closing transaction and evaluates the new blocks. |

A reorganisation that removes a deposit with 3 or more confirmations after a signature or a key
share was handed out is outside the protection of the protocol. The engine sends the deposit
transaction again if it holds it, and reports the event.

## Reactions to chain events

The engine performs these reactions on its own, while `musigd` runs. The caller is informed about
every event and every reaction.

"Spend of the seller's payout with the key path" means a confirmed transaction that spends this
output with a single signature and is not a warning transaction.

| Event (confirmed on chain) | Buyer's engine | Seller's engine |
|---|---|---|
| Deposit has 3 confirmations | Reports it. Allows the next steps. | Same |
| No deposit confirmation within the deposit timeout | Deposit abort | Same |
| A transaction that is not the deposit spends an input of the deposit (the deposit abort of either trader, or another spend) | `TradeClosed(DepositAborted)`; releases the coins | Same |
| Spend of the seller's payout with the key path, buyer not closed | Reads the seller's key share from the signature, imports the payout, `TradeClosed(Forced)` | Own swap transaction: `TradeClosed(Forced)` |
| Spend of the seller's payout with the key path, trade closed cooperatively | None. The seller spent his payout. The buyer's warning is impossible now. | None |
| Spend of the buyer's payout with the key path, trade closed | None | None. The buyer spent his payout. The seller's warning is impossible now. |
| Custom payout transaction | `TradeClosed(Custom)` | Same |
| Buyer's warning transaction | Own warning: waits for the claim lock time or for a transaction of the seller | Peer's warning: penalty if closed cooperatively, else redirect ([dispute.md](dispute.md)) |
| Seller's warning transaction | Peer's warning: penalty if closed cooperatively, else redirect | Own warning: waits for the claim lock time or for a transaction of the buyer |
| Redirect transaction of either trader | `TradeClosed` with the redirect closure type of its publisher | Same |
| Claim lock time of the trader's own warning has passed, escrow unspent | Claim, if the warning is the buyer's | Claim, if the warning is the seller's |
| Claim transaction of either trader | `TradeClosed` with the claim closure type of its publisher. The other trader's engine reports the loss. | Same |
| Penalty transaction of either trader | `TradeClosed` with the penalty closure type of its publisher. The other trader's engine reports the loss. | Same |
| Buyer's warning lock time ends in fewer blocks than the release safety margin, seller in `SellerReadyToRelease` | - | Publishes the swap transaction ([closing.md](closing.md)) |

Events that the engine does not expect, for example a spend of a payout with a signature from
which no valid key share can be read, are reported to the caller. The engine changes no state for
them.

How the engine tells a claim from a penalty: both spend the escrow output of a warning
transaction, and neither has a transaction ID that is known in advance. A spend through the
script path is the claim of the trader who published the warning. A spend through the key path
that is not the redirect transaction is the penalty of the other trader. The engine therefore
needs the witness of the transactions that it finds.

## Start-up

At start-up the engine evaluates, for every trade that is not final, all blocks since the height
at which the trader signed the deposit. It then performs the reactions for the events it finds, as
if they had just happened. Deadlines are computed from block heights, so they are correct after a
pause ([persistence-and-recovery.md](persistence-and-recovery.md)).

The engine uses what it knows about the chain for a trade only when it has evaluated all blocks
up to the chain tip for this trade. Before that, it changes no state from the chain and sends no
transaction for the trade: a transaction that it does not know can still be in a block that it
has not evaluated.

A request waits a few seconds for the evaluation of the newest block. The header of a block is
known a moment before the block is evaluated, and a caller that reacts at once to the
confirmation of a transaction would otherwise meet an engine that does not know about it.

The engine follows the chain from a block that the wallet knows, some blocks below the chain tip
of the wallet. When a trade or a key of the wallet needs earlier blocks, the engine follows the
chain from the earlier block.

The engine observes nothing while `musigd` does not run. `musigd` runs only while bisq2 runs. The
consequences for the user are stated in [post-trade-protection.md](post-trade-protection.md).
