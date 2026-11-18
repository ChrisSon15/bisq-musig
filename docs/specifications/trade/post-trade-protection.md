# Protection after a cooperative close

After a cooperative close no transaction was made. Each trader owns his payout in an output of
the deposit transaction. This file defines how the trader is protected until he has spent it.

## The risk

Both warning transactions stay valid as long as both payouts are unspent. A dishonest peer can
publish his warning transaction after the close. If the trader does not react within the claim
lock time, the peer claims all funds of the deposit ([dispute.md](dispute.md)).

The risk ends for both traders as soon as one payout is spent by a confirmed transaction.

## Rules

1. **The payout is imported into the wallet at close.** The engine gives the wallet the secret key
   of the payout and the script of the output. The wallet shows the payout as part of the balance
   and can spend it. The wallet starts to scan for this key above the height of the chain tip
   that it knew when the trader signed the deposit transaction, not at the first block of the
   chain ([../wallet/imported-keys.md](../wallet/imported-keys.md)).
2. **Imported coins are spent first.** Every transaction that the wallet builds spends all
   imported coins that are not reserved for an open trade. This rule cannot be switched off.
3. **The engine watches closed trades.** While `musigd` runs and both payouts of a closed trade
   are unspent, the engine watches the payouts. If the peer's warning transaction confirms, the
   engine publishes the penalty transaction at once.
4. **Imported coins are drained when the program ends.** When the user ends bisq2, bisq2 calls the
   close operation of the wallet service. The operation moves all imported coins that are not
   reserved to an address of the seed wallet, with one drain transaction.

Rationale for rule 2: every payment of the user then also ends the risk for the payouts he holds.
Rationale for rule 4: `musigd` does not run while bisq2 is closed, so nothing watches the payouts
in that time. A payout that is moved to the seed wallet cannot be taken by a warning transaction.

## The close operation

- It builds one drain transaction that spends all imported coins that are not reserved, to a new
  address of the seed wallet. The fee rate is the engine fee rate ([fees.md](fees.md)).
- It signs the transaction, stores it durably, and only then sends it.
- If there is nothing to drain, or the amount is not larger than the fee, it sends nothing and
  returns without an error.
- A second call while a drain transaction is stored and unconfirmed creates no second drain
  transaction.
- If no wallet is open, it returns a result that says so.
- Imported coins that are reserved for an open trade are not drained. The open trade continues at
  the next start.

The drain transaction is stored in the wallet as an unconfirmed transaction. The wallet service
sends every unconfirmed transaction that spends coins of the wallet again: once after the first
sync of a run of the program, and once after every new block
([../wallet/coin-reservation.md](../wallet/coin-reservation.md)). For the drain transaction this
means at the next start:

- confirmed: it is no longer unconfirmed, and nothing is sent;
- not confirmed: the wallet service sends it again, after the start and after every block.
  Exactly one drain transaction exists;
- one of its inputs was spent by another confirmed transaction: the wallet sees this spend,
  because the input is a coin of the wallet. The drain transaction is no longer valid for the
  wallet and is not sent. The engine evaluates the spend as a chain event of the trade.

A replacement of the drain transaction with a higher fee is defined in [fees.md](fees.md) and is
not implemented. That the wallet service sends all unconfirmed transactions of the wallet again,
and not only the drain transaction, is open point O20 in [README.md](README.md).

## Limits that the user must know

- The drain runs only when bisq2 calls the close operation. If the program crashes, is killed, or
  the computer loses power, nothing is drained.
- When the program ends, the drain transaction is sent but not confirmed. With a low fee it can
  stay unconfirmed.
- One drain transaction spends the payouts of several trades together. An observer of the chain
  can see that these trades belong to one user.
- While the program does not run, an open trade is not watched either.

Requirement for the user: while a trade is open, or a payout of a closed trade is neither spent
nor drained, the program must run often enough. After a warning transaction of the peer confirms,
the redirect or the penalty of the trader must confirm before the claim lock time ends. A trader
who stays offline for longer than the claim lock time can lose all funds of that trade to a
dishonest peer.

The user documentation of bisq2 must state this requirement.
