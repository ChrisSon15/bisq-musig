# Dispute path: warning, redirect, claim and penalty

This file defines how a trade ends when a trader publishes his warning transaction. The
transactions are defined in [transactions.md](transactions.md), the observation of the chain in
[chain-observation.md](chain-observation.md).

## Overview

A trader who cannot or does not want to close the trade with the peer publishes his warning
transaction. It moves all funds of the deposit into one escrow output. Three things can happen to
this escrow:

| Who acts | Transaction | Condition | Result |
|---|---|---|---|
| The peer | Redirect | The warning has 1 confirmation | All funds go to the burning men. A DAO arbitrator decides about a reimbursement. |
| The peer | Penalty | The peer holds the full key of the escrow, that is the trade was closed cooperatively before | The peer takes all funds. |
| The publisher | Claim | The warning has as many confirmations as the claim lock time, and the escrow is unspent | The publisher takes all funds. |

So a warning transaction is a question to the peer: answer within the claim lock time, or lose
the funds. A peer who is online answers with the redirect. A warning transaction that is published
after a cooperative close is an attempt to cheat and is answered with the penalty.

## Publishing the warning transaction

The warning transaction is published only when the caller asks for it. The engine never decides
on its own to start a dispute.

The engine refuses the request unless all of these hold:

- The trade is in `Deposit`, `BuyerReadyToRelease` or `CustomPayoutSigned`.
- The deposit has at least as many confirmations as the warning lock time. Before that, no node
  accepts the transaction.
- Both payouts are unspent, as far as the engine knows.

In particular the engine refuses:

- for the seller in `SellerReadyToRelease`: he released the key of the buyer's payout, so the
  buyer could take the whole escrow with the penalty;
- for a closed trade.

When the request is accepted, the state becomes `BuyersWarning` for the buyer or `SellersWarning`
for the seller. The engine publishes the transaction and raises its fee through the anchor output
when needed ([fees.md](fees.md)).

Both traders can publish their warning transaction. Both spend the same outputs, so only one
confirms. The trader whose warning did not confirm treats the confirmed one as the warning of the
peer.

## Reactions to a confirmed warning transaction of the peer

The engine reacts on its own, without a request of the caller. It reacts when the warning
transaction has 1 confirmation.

| Situation of the trader | Reaction | State after the reaction confirmed |
|---|---|---|
| `TradeClosed(Cooperative)`: the trader holds the full key of the escrow | Publish the penalty transaction at once | `TradeClosed(BuyersPenaltyTx)` or `TradeClosed(SellersPenaltyTx)`, named after the trader |
| Any other state after `Init` | Publish the redirect transaction | `TradeClosed(BuyersRedirect)` or `TradeClosed(SellersRedirect)`, named after the trader |

Details:

- When the warning of the peer confirms, the state becomes `SellersWarning` (seen by the buyer)
  or `BuyersWarning` (seen by the seller), unless the trade is closed.
- The redirect transaction has a lock time of 1 block. It can confirm in the block after the
  warning at the earliest.
- The trader's own unconfirmed transactions that spend a payout are void when the peer's warning
  confirms. This applies to his own warning transaction, to the seller's swap transaction and to
  a custom payout transaction. The engine stops sending them.
- The seller in `SellerReadyToRelease` has released his key share of `P'` but does not hold the
  key of `Q'`. He cannot use the penalty. His engine publishes the redirect transaction.
- The reaction must confirm before the claim of the peer becomes valid. The engine raises the fee
  of the reaction when it does not confirm ([fees.md](fees.md)).

If the claim of the peer confirms before the reaction, the trader has lost the funds. The state
becomes `TradeClosed(BuyersClaim)` or `TradeClosed(SellersClaim)`, named after the peer, and the
engine reports the loss.

## After the trader's own warning transaction confirmed

The engine watches the escrow output.

| Event | Reaction | State after |
|---|---|---|
| The peer's redirect transaction confirms | None | `TradeClosed(SellersRedirect)` or `TradeClosed(BuyersRedirect)`, named after the peer |
| The warning has as many confirmations as the claim lock time and the escrow is unspent | Build, sign and publish the claim transaction | `TradeClosed(BuyersClaim)` or `TradeClosed(SellersClaim)`, named after the trader, when the claim confirmed |
| The peer's penalty transaction confirms | None. The engine reports the loss. | `TradeClosed(SellersPenaltyTx)` or `TradeClosed(BuyersPenaltyTx)`, named after the peer |

Details:

- The engine does not publish the claim before the claim lock time has passed. No node would
  accept it.
- The claim and the redirect of the peer can both be valid after the claim lock time. The one
  that confirms decides the outcome.
- The claim pays the fee rate of the moment and can be replaced with a higher fee.
- The penalty of the peer is possible only if the trader published his warning after a cooperative
  close. The engine refuses that request, so this event shows that the warning was published in
  another way, for example by a second copy of the trade data.

### The claim signature

The claim spends the escrow through its script leaf:

- The witness contains the signature, the script and the control block of the leaf.
- The signature is made with the trader's claim key. For the buyer this is his key share of `P'`,
  for the seller his key share of `Q'`. The script contains the x-only form of the public key
  share, so the secret key is negated for signing when the public key share has an odd Y
  coordinate.
- The sequence number of the input is the claim lock time.

The peer cannot create this signature, because the claim keys are never given away.

## Outcomes

| Trader A publishes his warning | Trader B | Funds |
|---|---|---|
| Before any close | Redirects in time | Burning men; reimbursement by the arbitrator |
| Before any close | Does not react within the claim lock time | A, minus fees |
| After a cooperative close | Uses the penalty in time | B, minus fees |
| After a cooperative close | Does not react within the claim lock time | A, minus fees |

The last row is the reason why a trader must not stay offline for longer than the claim lock time
while a payout of a closed trade is unspent ([post-trade-protection.md](post-trade-protection.md)).

## When a warning transaction is impossible

A warning transaction spends both payouts. As soon as one payout is spent by another confirmed
transaction, neither warning transaction can ever confirm. This is the case after:

- a confirmed swap transaction;
- a confirmed custom payout transaction;
- a confirmed transaction with which a trader spent his payout after a cooperative close,
  including the drain transaction.
