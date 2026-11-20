# Imported keys of payouts

After a close a trader holds the complete key of his payout output. The key does not come from
the seed of the wallet. This file defines how the wallet handles such keys. The reasons for the
protection after a close are in
[../trade/post-trade-protection.md](../trade/post-trade-protection.md).

## Import

1. **The engine imports the key at the close**, together with the script of the payout output.
   The wallet then shows the payout as part of the balance and can spend it.
2. **The key is stored at once**, in the encrypted main database of the wallet.
3. **An imported key is unique.** A second import of the same key is refused.
4. **The import is the last step of the close.** If it fails, the trade is not closed, and the
   close can be repeated.
5. **The address of an imported key is an address of the wallet.** The list of the addresses of
   the wallet (`GetWalletAddresses`) contains it, after the addresses of the seed. It stays in
   the list after its coin is spent.

Rule 5 is a decision of the project owner of 2026-11-20. Consequence: the list contains the
address of every coin that the wallet reports. A caller cannot tell from the list whether an
address belongs to the seed or to an imported key.

## Start of the scan

The engine passes a height with the import: the height of the chain tip that the wallet knew when
the trader signed the deposit transaction. The deposit transaction cannot be confirmed at or
below this height.

- The wallet looks for coins of the key only in blocks above this height.
- An import must not make the scan of the whole wallet start at the first block of the chain.

Rationale: the wallet keeps a separate chain view for every imported key. A new view that knows
only the first block of the chain makes the next scan with compact block filters start there, for
all keys of the wallet. On mainnet this is a download of all filters after every closed trade.
This was finding F19.

## Spending order

1. **Every transaction that the wallet builds spends all imported coins that are not reserved.**
   This holds for payments, for the deposit of the next trade, and for every other transaction.
   The rule cannot be switched off.
2. **Imported coins that are reserved for an open trade, and coins that are not mature, are left
   out.**
3. **The caller cannot select the coins of a transaction.** The wallet selects them for every
   transaction, and no request of the wallet service takes a list of coins. So rule 1 has no
   exception.

Rationale for rule 3: a payment from coins that the user selected by hand could leave an imported
payout unspent, and the protection of rule 1 would depend on the user's choice.

Consequences that the user must know: a payment after a close links the payout with the payment
on the chain, and it can be larger than a payment from the seed wallet alone, so its fee is
higher.

## Signing

The wallet signs a coin of an imported key in every transaction that spends it, from what the
wallet knows itself: the secret key and the script of the output. It does not need the Taproot
key data of the coin in the PSBT that it is given.

Rationale: the trade protocol removes this data from the trader's half of the deposit
transaction before the half goes to the peer, and signs the deposit transaction later. Because
imported coins are spent first, the deposit of the next trade after a cooperative close spends
the payout of the earlier trade. A wallet that needed the data in the PSBT could not sign this
deposit.

## Drain

The close operation of the wallet service moves all imported coins that are not reserved to one
new address of the seed wallet. The operation is defined in
[../trade/post-trade-protection.md](../trade/post-trade-protection.md). The wallet part is:

- build one transaction with the given fee rate, sign it, record it as unconfirmed and store it,
  before it is handed out for sending;
- if there is nothing to move, or the coins do not pay for the fee and an output above the dust
  limit, do nothing and report that;
- a second call does not create a second transaction, because the coins of the first one count
  as spent.

The fee of every transaction with imported coins must reach the requested fee rate. The weight of
the witness of an imported coin is 66 weight units (a Taproot key spend with the default signature
hash type).

## Storage

- The secret keys of the imported keys are stored in the encrypted main database.
- The chain view of every imported key is stored in a file of its own. These files are encrypted.
  Their key is random, it is stored in the encrypted main database, and it does not depend on
  the password of the wallet.

Rationale: the files contain the descriptors of the payout outputs, with the keys of the peer, and
the transactions of closed trades. Unencrypted files would show the trade history to everyone who
can read the disk. This was finding F17. A key that does not depend on the password keeps a
password change limited to one file, so an interrupted password change cannot leave files with
different keys.
