# Fees

This file defines which fee rate each transaction of a trade uses, and how the fee of an
unconfirmed transaction is raised.

## Fee rates

| Fee rate | Source | Used for |
|---|---|---|
| Deposit fee rate | Trade parameter | Deposit transaction |
| Prepared fee rate | Trade parameter | Swap, warning and redirect transactions; penalty transaction unless another rate is set at close |
| Custom payout fee rate | Request of the caller, the same for both traders | Custom payout transaction |
| Engine fee rate | The engine | Claim, deposit abort, drain, and every transaction that raises the fee of another one |

The engine applies sanity limits to the deposit fee rate and the prepared fee rate that the caller
passes. The limits are open point O12 in [README.md](README.md).

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

Rules for the schedule:

- The first fee rate of a child or of a replaceable transaction is the engine fee rate.
- Each step raises the fee rate. A replacement pays at least the additional fee that the relay
  rules require.
- For the redirect and the penalty the schedule is chosen so that the transaction confirms before
  the claim lock time of the peer's warning ends.
- The engine never spends more on fees than the amount that the trader would lose without the
  transaction.

The number of blocks between two steps, the size of a step and the upper limit are open point O9
in [README.md](README.md).

## Protection against a blocked fee increase

A peer must not be able to prevent that the trader raises the fee of his warning transaction. The
protection is the redirect lock time of 1 block ([transactions.md](transactions.md)): while the
warning transaction is unconfirmed, the only transaction that can spend one of its outputs is the
trader's own child transaction for the anchor.

The trader's engine must also react when the warning transaction of the peer confirms instead of
his own ([dispute.md](dispute.md)). Without this reaction a peer could replace the trader's
warning transaction with his own and wait for the claim lock time.
