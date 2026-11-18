# Fees

This file defines which fee rate each transaction of a trade uses, and how the fee of an
unconfirmed transaction is raised.

## Fee rates

| Fee rate | Source | Used for |
|---|---|---|
| Deposit fee rate | Trade parameter | Deposit transaction |
| Prepared fee rate | Trade parameter | Swap, warning and redirect transactions; penalty transaction unless another rate is set at close |
| Custom payout fee rate | Request of the caller, the same for both traders | Custom payout transaction |
| Engine fee rate | The engine | Claim, deposit abort, drain, and every transaction that raises the fee of another one. The deposit abort pays more if the fee of the deposit transaction requires it ([transactions.md](transactions.md)). |

The engine applies limits to the deposit fee rate and the prepared fee rate that the caller
passes. A trade with a fee rate outside the limits is refused before any coin is reserved.

| Limit | Value | Reason |
|---|---|---|
| Lowest fee rate | 1 sat per vbyte | The fixed minimum relay fee of the nodes. A transaction below it is not accepted, also not together with a child (see "Raising a fee"). |
| Highest fee rate | 1000 sat per vbyte | It stops a caller that passes a fee rate in a wrong unit. |

The values are open point O12 in [README.md](README.md).

The deposit fee rate and the prepared fee rate are fixed when the trade starts. The pre-signed
transactions can be published weeks later. Their fee can therefore be too low at that time, and
the protocol needs the ways to raise a fee that are described below.

### Engine fee rate

Every transaction that the engine builds on its own takes its fee rate from one function of the
engine. No other place in the code contains a fee rate for these transactions.

For now this function returns a fixed value of 10 sat per vbyte. The real source of the fee rate
is supplied later and is not part of this specification.

Limit of the fixed value: when the network needs more than 10 sat per vbyte, these transactions
can stay unconfirmed. This includes the transactions that have a deadline.

Rationale for one function: the source can be replaced later in one place, and a test can check
that every transaction of the engine uses it.

## Who pays

| Transaction | Paid by |
|---|---|
| Deposit | Each trader for his own inputs and outputs and half of the fixed part |
| Swap | Seller, from his deposit `Ds` |
| Warning, redirect | From the escrow, so from the funds of both traders |
| Claim, penalty | From the escrow, so by the trader who receives it |
| Custom payout | Half by each trader; the buyer pays 1 sat more if the fee is odd |
| Deposit abort, drain, fee-raising transactions | The trader who sends them |

## Raising a fee

The engine has no view of a mempool. It decides by block count: a transaction that the engine sent
and that is still unconfirmed after a number of blocks gets a higher fee.

| Transaction | Way to raise the fee |
|---|---|
| Deposit | A child transaction that spends the trader's change output. Without a change output: the deposit abort. |
| Warning | A child transaction that spends the trader's anchor output |
| Redirect | A child transaction that spends the trader's anchor output |
| Swap | A child transaction that spends the output of the swap transaction |
| Penalty | A child transaction that spends the output of the penalty transaction |
| Claim | Replacement by a new claim transaction with a higher fee |
| Deposit abort | Replacement with a higher fee |
| Drain | Replacement with a higher fee |
| Custom payout | A child transaction that spends the trader's output |

A child transaction and its parent are sent together as one package, so that a parent whose own
fee is below the minimum of a node is still accepted. This specification assumes that the nodes
of the peer-to-peer network accept such a package of one parent and one child.

The assumption holds for the minimum fee that a node raises while its mempool is full. It does
not hold for the fixed minimum relay fee of a node: a parent below this fee is not accepted in a
package either. A transaction that is signed in advance must therefore have a fee rate of at least
the fixed minimum relay fee of the nodes, which is 1 sat/vB by default in Bitcoin Core 29.0.

Both statements are tested with Bitcoin Core 29.0 only (the two tests J7 of
`chain/tests/chain_client.rs`). Other nodes can behave differently.

### The fee schedule

The fee rate of a step is the engine fee rate, multiplied by 3/2 for every step. Step 0 is the
engine fee rate itself. The last step is step 10, which is about 58 times the engine fee rate.

The step of a transaction follows from the number of blocks for which the transaction could have
been in a block and is not:

| Transaction | Counted from | Blocks per step |
|---|---|---|
| Deposit | The block at which the engine first saw the chain without the deposit ([setup-and-deposit.md](setup-and-deposit.md)) | 6 |
| Deposit abort | The end of the deposit timeout | 6 |
| Warning, custom payout | The block at which the request that makes the engine send the transaction was processed | 6 |
| Swap | The block at which the seller closed by force, by a request or by the engine | The number of blocks that were left before the buyer's warning became valid, divided by 20, at least 1 |
| Redirect, penalty | The first confirmation of the peer's warning | The claim lock time divided by 20, at least 1 |
| Claim | The end of the claim lock time | 6 |

A transaction with a deadline (swap, redirect, penalty) therefore reaches the last step after
half of the time that it has.

The engine stores no list of the transactions that it sent. Every value of the table follows
from the stored state of the trade and from the chain, so the schedule continues after a
restart. The block of the last change of the state is part of the stored trade.

Rules:

- A transaction with a deadline is raised to the fee rate of its step from step 0 on: if it
  pays less than the engine fee rate itself, it is sent with a child from the start.
- A transaction without a deadline is first sent as it is, with the fee rate that the traders
  agreed on. It gets a child from step 1 on.
- A transaction that pays the fee rate of its step itself gets no child.
- A child is built by the wallet. It spends the output of the transaction that belongs to the
  wallet, and coins of the wallet if this output does not pay for the fee. It pays so much that
  the transaction and the child together reach the fee rate of the step. The wallet records the
  transaction and the child ([../wallet/coin-reservation.md](../wallet/coin-reservation.md)).
- At a new step the wallet builds a new child, which replaces the earlier one. A step raises the
  fee rate by one half, which is more than the additional fee that the relay rules require for
  a replacement.
- The claim and the deposit abort are built again by the engine with the fee rate of the step.
  The deposit abort pays the higher of this fee and the fee that replaces the deposit
  ([transactions.md](transactions.md)), so its first steps can give the same transaction.
- A child never pays more than the amount that the transaction spends. This is the upper limit
  for "the engine never spends more on fees than the trader would lose without the
  transaction". A lower limit for each role and each transaction is not specified.
- A transaction that the engine sends for the first time in a run of the program is sent alone
  first, and then together with its child. A node takes some seconds longer to accept a package
  than a single transaction, and a reaction with a deadline must not wait for that. A node that
  refuses the single transaction for its fee accepts it afterwards in the package.
- If the wallet cannot build a child, for example because it holds no coin that pays for it, the
  engine sends the transaction without a new child, reports this as an event of the trade, and
  tries again at the next step.
- When a transaction of the trader can never confirm, because a confirmed transaction spent one
  of the outputs that it spends, the engine tells the wallet to forget it together with its
  child. The coins that the child spent are free again.

Not specified here: raising the fee of the drain transaction and of a payment of the wallet. The
wallet service sends them again and does not raise their fee.

The values of the schedule (blocks per step, size of a step, number of steps) are open point O9 in
[README.md](README.md).

## Protection against a blocked fee increase

A peer must not be able to prevent that the trader raises the fee of his warning transaction. The
protection is the redirect lock time of 1 block ([transactions.md](transactions.md)): while the
warning transaction is unconfirmed, the only transaction that can spend one of its outputs is the
trader's own child transaction for the anchor.

The trader's engine must also react when the warning transaction of the peer confirms instead of
his own ([dispute.md](dispute.md)). Without this reaction a peer could replace the trader's
warning transaction with his own and wait for the claim lock time.
