# Reservation of coins and recording of transactions

This file defines how the wallet keeps the coins of a deposit away from other uses, and how it
learns about a transaction that it did not build itself.

## Reservation

1. **The coins of a deposit are reserved from the moment the trader's half of the deposit
   transaction exists.** A reserved coin is not selected for another trade, for a payment, or for
   the drain.
2. **A request that is refused reserves nothing.** The engine reserves the coins as the last step
   of the request that creates the half deposit.
3. **The reservation is stored in the wallet.** It survives a restart.
4. **The reservation ends in one of two ways:**
   - The trade is abandoned before the trader signed the deposit transaction. The coins are free
     again.
   - The deposit transaction is recorded in the wallet or confirmed. The coins are spent, and a
     spent coin is not listed as reserved.
5. **After the trader signed the deposit transaction, the coins are never released.** The peer
   holds the signature and can publish the deposit at any time. The only way to free the coins
   then is to spend them with another transaction
   ([../trade/setup-and-deposit.md](../trade/setup-and-deposit.md), deposit abort).
6. **The balance that the wallet offers for spending does not contain reserved coins.** The
   wallet reports the reserved amount separately.

Rationale: without a reservation two trades, or a trade and a payment, can select the same coin.
One of the two transactions then never confirms. This was finding F13.

### Coins that stay reserved without a trade

The trade store is in memory only. If `musigd` stops while a trade is in its setup, the trade is
gone but its coins stay reserved in the wallet database. This is accepted until the trade store
exists; then the engine releases such coins at the next start
([../trade/persistence-and-recovery.md](../trade/persistence-and-recovery.md)).

Until then, and for any other reservation without a trade, the wallet service offers a release by
hand:

1. **The reserved coins can be listed.** The list of the unspent coins of the wallet marks every
   reserved coin.
2. **A reserved coin can be released by a request that names the coin.** The release is stored.
   A request for a coin that is not reserved succeeds and reports that nothing was released.
3. **A coin that a trade known to the engine has reserved is refused.** Such a coin is released
   only through its trade, by abandoning the trade, and only while rule 5 above allows it.

Limit that the user must know: the engine knows only the trades of the running program. If
`musigd` stopped after the trader signed a deposit transaction, the engine has forgotten that
trade, and a release by hand is not refused, although the peer can still publish the deposit.
A payment with the released coin then competes with the deposit: one of the two transactions
confirms. No funds are lost by this, but the trade can fail. This limit ends with the trade
store.

## Recording a transaction

The wallet offers an operation that records a signed transaction as unconfirmed, also if the
wallet did not build it.

- From then on the coins of the wallet that the transaction spends count as spent, and its
  outputs to the wallet count as incoming.
- The record is stored.
- The caller sends the transaction to the network. The wallet does not.

The engine records the deposit transaction when it publishes it.

Rationale: the wallet sees the chain through compact block filters, which report confirmed
transactions only. Without the record the wallet would believe that the coins of the deposit are
unspent until a block confirms the deposit. This was finding F14.

A transaction that is recorded but never reaches the network leaves its coins marked as spent.
This is the safe direction: the coins can be recovered, and they cannot be spent twice.
