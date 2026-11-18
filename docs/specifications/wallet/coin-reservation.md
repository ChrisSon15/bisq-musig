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

If `musigd` stops while a trade is in its setup, the engine abandons the trade at the next start
and releases its coins. It also releases a reserved coin that no stored trade reserves
([../trade/persistence-and-recovery.md](../trade/persistence-and-recovery.md)).

For any other reservation without a trade, the wallet service offers a release by hand:

1. **The reserved coins can be listed.** The list of the unspent coins of the wallet marks every
   reserved coin.
2. **A reserved coin can be released by a request that names the coin.** The release is stored.
   A request for a coin that is not reserved succeeds and reports that nothing was released.
3. **A coin that a trade known to the engine has reserved is refused.** Such a coin is released
   only through its trade, by abandoning the trade, and only while rule 5 above allows it.

4. **No coin is released by hand while the trades of the wallet could not be loaded.** The
   engine then does not know which coins belong to a trade.

The engine knows the trades of the wallet after a restart, because they are stored in the wallet
database. Rule 3 therefore also holds for a trade that was set up before the restart.

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

A transaction that is recorded and not confirmed leaves its coins marked as spent. This is the
safe direction: the coins cannot be spent twice. Two rules keep such a record from staying for
ever.

### Sending recorded transactions again

The owner of the wallet sends every unconfirmed transaction that spends coins of the wallet to
the network again: after the first sync of a run of the program, and after every new block.

- The wallet names these transactions: payments, drain transactions and recorded transactions of
  trades.
- A transaction leaves the list when it is confirmed, when the wallet sees a confirmed transaction
  that spends one of the same coins, or when it is forgotten.

Rationale: a broadcast over the peer-to-peer network has no result. The program can end before a
peer took the transaction, as after the drain at the close of the wallet, and a node can drop a
transaction.

### Forgetting a recorded transaction

The wallet offers an operation that removes a recorded unconfirmed transaction. The coins that it
spends count as unspent again, and its outputs no longer count as incoming. A transaction that is
unknown or confirmed is left as it is.

The engine uses the operation for the deposit transaction when the deposit can never confirm
([../trade/setup-and-deposit.md](../trade/setup-and-deposit.md)).

Rationale: the wallet notices on its own when another confirmed transaction spends one of its
coins. It cannot notice a transaction that spends a coin of somebody else. The transaction that
makes a deposit impossible can spend an input of the peer. Without this operation the wallet of
the trader who published the deposit would count his coins as spent for ever.
