# Parameters, constants and limits

This file defines the values that a trade depends on: the parameters that the two traders agree
on, the constants of the protocol, the configuration values of the engine, and the limits that the
engine enforces.

## Trade parameters

The trade parameters are part of the trade contract of the two traders. The caller (bisq2) passes
them to the engine at trade start. Both traders must use the same values.

| Parameter | Unit | Meaning |
|---|---|---|
| Trade amount `T` | sat | Amount that the buyer buys |
| Buyer's security deposit `Db` | sat | |
| Seller's security deposit `Ds` | sat | |
| Deposit fee rate | sat per 1000 weight units | Fee rate of the deposit transaction |
| Prepared fee rate | sat per 1000 weight units | Fee rate of the swap, warning and redirect transactions |
| Warning lock time | blocks | Relative lock time of both warning transactions, counted from the confirmation of the deposit |
| Claim lock time | blocks | Relative lock time of both claim transactions, counted from the confirmation of the warning |
| Trade fee receiver | address and amount | Optional. At most one output in the deposit transaction, paid by the seller |
| Redirection receivers | list of address and amount | Outputs of both redirect transactions |

The engine has no values of its own for the lock times. There are no values per network.

Rationale: the lock times depend on the payment method, because the warning lock time is also the
time that the seller has to receive the payment or to start mediation. Values that are fixed in
the engine would not fit all payment methods, and tests need short lock times.

### How the traders agree

The engine does not trust the caller that the peer uses the same parameters. The agreement is
enforced by the signatures: every pre-signed transaction depends on the parameters, and a partial
signature of the peer is valid only for the transaction that the trader computed himself. If the
parameters differ, the aggregation of the signatures fails and the deposit is not signed
([setup-and-deposit.md](setup-and-deposit.md)).

## Constants of the protocol

| Constant | Value | Rationale |
|---|---|---|
| Redirect lock time | 1 block | Prevents that the peer blocks a fee increase of the warning transaction ([transactions.md](transactions.md)) |
| Minimum confirmations of the deposit (`MIN_CONFIRMATIONS`) | 3 | Below this depth the deposit can still disappear in a reorganisation. No signature for the swap transaction and no key share leaves the engine before. |
| Anchor amount | 330 sat | Dust limit of a P2TR output |
| Maximum number of inputs and of outputs of one half of the deposit | 126 each | Keeps the size fields of the merged transaction at one byte, so that the fee calculation of each half stays exact |

These values are the same for every trade and every network. A trade parameter cannot change them.

## Configuration values of the engine

These values belong to one trader's engine. The peer does not know them.

| Value | Unit | Meaning |
|---|---|---|
| Deposit timeout | blocks | Number of blocks without a confirmed deposit after which the trader aborts the deposit ([setup-and-deposit.md](setup-and-deposit.md)) |
| Release safety margin | blocks | The seller does not release the payout when fewer blocks than this remain before the buyer's warning lock time ends ([closing.md](closing.md)) |
| Engine fee rate | sat per vbyte | Fee rate of the transactions that the engine builds on its own ([fees.md](fees.md)) |

Each value has a default and can be changed in the configuration of `musigd`. The default values
are open point O7 in [README.md](README.md).

## Limits

The engine rejects a trade whose parameters break one of these limits. The check happens before
any coin of the wallet is reserved.

Lock times:

- The warning lock time and the claim lock time are block counts between 1 and 65535 (the range
  of a relative lock time in blocks, BIP68).
- The warning lock time is larger than `MIN_CONFIRMATIONS` plus the release safety margin.
  Otherwise no time would be left in which the seller may release.
- The claim lock time is larger than the redirect lock time. Otherwise the peer could not publish
  a redirect transaction before the claim becomes valid.

Amounts (all conditions use the prepared fee rate and the weights of
[transactions.md](transactions.md)):

- No output of any transaction of the trade is below the dust limit of its script type. This
  includes both payouts, the trade fee output, the output of the swap transaction, the escrow
  outputs, and every output of the redirect transactions.
- The escrow amount covers the penalty fee and leaves an output above the dust limit.
- Every amount and every fee rate fits into a signed 64-bit number.

These conditions define the minimum trade amount and the minimum deposits. They depend on the fee
rates, so the engine computes them for every trade.

Redirection receivers:

- The sum of the receiver amounts, plus the fee for the receiver outputs at the prepared fee
  rate, equals the amount that the redirect transaction can distribute. The amount to distribute
  is the escrow amount minus the anchor amount minus the fee for the fixed part of the redirect
  transaction. The sum may be lower than this amount by less than 1 satoshi, never higher.
- The signed redirect transaction is not heavier than a standard transaction (400,000 weight
  units).

The engine reports the amount to distribute to the caller during setup, in thousandths of a
satoshi, so that the caller can compute the receiver amounts.

Addresses:

- Every address belongs to the network of the trader's wallet.

Sizes:

- The engine limits the size of every message field and the length of every list. Data of the
  peer above a limit terminates the trade ([trade-states.md](trade-states.md)). The values of the
  limits are open point O10 in [README.md](README.md).
