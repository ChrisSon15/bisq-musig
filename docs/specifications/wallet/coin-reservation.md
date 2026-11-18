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

### What is not covered yet

The trade store is in memory only. If `musigd` stops while a trade is in its setup, the trade is
gone but its coins stay reserved in the wallet database. No request releases them. This is open
point W2 in [README.md](README.md); the specification of the trade store defines the release at
the next start ([../trade/persistence-and-recovery.md](../trade/persistence-and-recovery.md)).

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
