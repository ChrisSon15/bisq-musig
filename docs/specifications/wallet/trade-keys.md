# Keys and addresses of a trade

A trade takes two kinds of secrets from the wallet: the key shares of the trader, and keys of the
seed wallet for the script key and the addresses of the trade.

## Key shares

The wallet offers one operation that takes a trade ID and returns the two secret key shares of
this trade. The derivation is defined in
[../trade/key-derivation.md](../trade/key-derivation.md).

- The seed and the trade root secret do not leave the wallet.
- The operation needs the open wallet.
- The same wallet and the same trade ID always give the same key shares.

## Keys of the seed wallet for a trade

A trade takes from the wallet:

- one key for the script of the two payout outputs of the deposit (the script key);
- one address for the anchor output of the trader's warning transaction;
- one address for the anchor output of the trader's redirect transaction;
- one payout address, for the claim transaction, the penalty transaction and a custom payout;
- for the seller, one address for the output of the swap transaction.

Rules:

1. **A key of a trade is never issued again.** Not to another trade, and not as a receive address
   of the user.
2. **The wallet stores which keys it issued to trades**, before the key leaves the wallet. The
   record survives a restart.
3. **The rule for receive addresses of the user does not apply to trades.** For the user, the
   wallet offers unused addresses again when many addresses in a row are unused. A trade always
   gets a new key.

Rationale: every pre-signed transaction of a trade pays to these addresses or uses the script
key, and the transactions stay valid after the trade ends. If two trades shared an address, the
funds of two trades could arrive on one address, and an observer of the chain could link the
trades. Most keys of a trade never receive funds, so the rule "offer an unused address again"
would hand them out again after a few trades. This was finding F12.

Consequence: the keys of trades form long rows of keys without funds in the seed wallet. A wallet
that is restored from its seed words must still find funds behind such a row. This rule is not
specified yet: restoring is designed after the work packages of the plan are finished (point W1
in [README.md](README.md)).
