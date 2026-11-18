# Trade setup and deposit

This file defines how two traders set up a trade: the messages, the checks of the peer's data, the
signing and publication of the deposit transaction, and the ways a setup can end without a trade.

## Messages

The traders exchange four messages. The taker starts.

| Message | From | Content |
|---|---|---|
| A | Taker | The taker's public key shares of `P'` and `Q'`, the taker's script key |
| B | Maker | The maker's public key shares and script key; the maker's half of the deposit; the maker's addresses; the maker's nonces |
| C | Taker | The taker's half of the deposit; the taker's addresses; the taker's nonces; the taker's partial signatures for the maker's transactions |
| D | Maker | The maker's partial signatures for the taker's transactions; the deposit transaction with the maker's signatures |

Addresses of a trader: the anchor address of his warning transaction, the anchor address of his
redirect transaction, and his payout address. The payout address receives the trader's output of
a custom payout and of his penalty transaction.

Nonces of a trader: one MuSig2 nonce for each signature that the two traders create together.
These are: the swap transaction, the two inputs of the buyer's warning, the two inputs of the
seller's warning, the buyer's redirect, the seller's redirect. That is seven nonces.

Partial signatures of a trader for the peer's transactions: the two inputs of the peer's warning
and the peer's redirect.

The swap transaction is special:

- The seller sends, with his partial signatures, the signature hash of the swap transaction and
  his partial signature for it.
- The buyer creates his partial signature for this signature hash during the setup but does not
  send it. He sends it when he has started the payment ([closing.md](closing.md)).

The messages travel through bisq2. The engine sees them as data of the requests of its caller.

## Steps of one trader

| Step | Input | The engine does | Output |
|---|---|---|---|
| 1. Create the trade | Trade ID, role, trade parameters | Checks the parameters ([parameters.md](parameters.md)). Derives the key shares. Takes a script key from the wallet. | Own public key shares and script key |
| 2. Key shares of the peer | Message A or B (keys) | Checks the keys. Aggregates `P'` and `Q'`. Computes all output scripts. Takes the addresses from the wallet. Selects and reserves coins and builds the own half of the deposit. Creates the nonces. | Own half of the deposit, addresses, nonces |
| 3. Half deposit, addresses and nonces of the peer | Message B or C | Checks all data. Builds the deposit transaction and all pre-signed transactions. Creates the partial signatures. | Own partial signatures; the five transaction IDs of deposit, warnings and redirects |
| 4. Partial signatures of the peer | Message C or D | Checks every signature. Completes the own warning and redirect transaction. The buyer completes and checks the adaptor signature of the swap transaction. Stores the trade. Signs the own inputs of the deposit. State becomes `Deposit`. | Deposit transaction with the own signatures |
| 5. Publish | Deposit transaction with the peer's signatures | Checks that it is the same transaction and that every input is signed. Records it in the wallet. Sends it to the network. | Events about the confirmation |

The caller should compare the five transaction IDs of step 3 with the IDs that the peer reports.
They identify the contract of the trade. The engine does not rely on this comparison: step 4 fails
if the transactions differ.

A trade ID can be used only once ([key-derivation.md](key-derivation.md)). Step 1 with a known
trade ID is refused and does not change the existing trade.

## Checks of the peer's data

The order of checks and the consequences of a failed check are defined in
[trade-states.md](trade-states.md). Nothing of a request with a failed check is stored.

All checks of this section concern data that the peer created, except the limits of the
redirection receivers, which the caller supplies. An unmodified client of the peer passes all of
them. A failed check of the peer's data therefore terminates the trade, and the engine logs the
reason in detail. A setup that is terminated is abandoned.

Key shares and script key:

- Each key share is a valid point of the curve, and the script key is a valid x-only key.
- The peer's two key shares differ from each other.
- No key share of the peer equals a key share of the trader.
- The peer's script key differs from the trader's script key.

Half of the deposit:

- Its first output is a placeholder: an `OP_RETURN` output with 27 bytes of data and the amount
  that the peer must pay into the deposit. This is `Db` for the buyer and `T + Ds` for the
  seller.
- The seller's half contains the trade fee output directly after the placeholder, with the agreed
  address and amount. The buyer's half contains none.
- Every input spends a P2TR or a P2WPKH output, names the spent output and its amount, and has the
  sequence number `0xFFFFFFFD`.
- It has at most 126 inputs and at most 126 outputs.
- It pays at least the fee for its own weight at the deposit fee rate.
- It contains nothing else: no signatures, no further fields.
- No input of the peer is also an input of the trader.

Rationale for the placeholder: the 27 random bytes of both placeholders decide the shuffle of the
deposit transaction. Nobody else knows them, so nobody else can compute the order.

Addresses and receivers:

- Every address belongs to the network of the wallet.
- The redirection receivers satisfy the limits of [parameters.md](parameters.md).

Nonces:

- Every nonce is a valid MuSig2 public nonce.
- All nonces of the peer differ from each other and from all nonces of the trader for this trade.

Partial signatures:

- Each partial signature of the peer is valid for the transaction that the trader computed
  himself, with the aggregated nonce of this signature.
- For the buyer: the seller's partial signature for the swap transaction gives a valid adaptor
  signature for the signature hash that the seller sent, with the seller's public key share of
  `P'` as adaptor point.

Because every signature is checked against the trader's own transactions, a peer that used other
amounts, fee rates, lock times, addresses or receivers fails this check.

## Signing the deposit

The trader signs his deposit inputs only when all of these hold:

- his own warning and redirect transaction are fully signed;
- the buyer holds the checked adaptor signature of the swap transaction; the seller holds the swap
  transaction and his partial signature for it;
- the trade, with these transactions, is stored durably
  ([persistence-and-recovery.md](persistence-and-recovery.md)).

Rationale: from the moment his signature is out, the trader's coins can be locked in the deposit.
He must then be able to leave the deposit alone, with the warning transaction, also after a
restart.

The trader's signatures cover only his own inputs. The peer's inputs are not checked on chain. A
deposit with an invalid or already spent input of the peer never confirms and ends with the abort
below.

## Publication and confirmation

The trader who holds all signatures publishes the deposit. In the normal flow this is the taker.

The engine reports the deposit as confirmed when it has `MIN_CONFIRMATIONS` (3) confirmations.
Before that, the engine refuses every request that hands out the buyer's partial signature for the
swap transaction, a key share, or a signature for a custom payout.

If a reorganisation lowers the depth below `MIN_CONFIRMATIONS` before the buyer started the
payment, the deposit counts as unconfirmed again ([chain-observation.md](chain-observation.md)).

## Ending a setup without a trade

Before the trader signed the deposit (`Init`):

- The trade is abandoned when the caller cancels it, when it is terminated, or when the engine
  restarts. Nonces are not stored, so a setup cannot continue after a restart.
- The reserved coins are released. No funds moved.

After the trader signed the deposit (`Deposit`), and the deposit is not confirmed:

- The engine counts the blocks since it signed. When the deposit timeout has passed without a
  confirmation of the deposit, the engine publishes the deposit abort transaction
  ([transactions.md](transactions.md)).
- The engine cannot see unconfirmed transactions. It therefore watches for the confirmation of
  either transaction and follows the one that confirms:
  - the abort confirms: the trade is closed as `DepositAborted`, the coins are free again;
  - the deposit confirms: the trade continues in `Deposit`.
- While neither is confirmed, the engine sends the abort transaction again and can raise its fee
  ([fees.md](fees.md)).

Rationale: without the abort, a peer who never publishes the deposit would keep the trader's coins
reserved for ever, and could publish the deposit at any later time.
