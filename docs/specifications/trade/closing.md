# Closing a trade without a dispute

This file defines the three ways a trade closes when no warning transaction is used: the
cooperative close, the forced close with the swap transaction, and the custom payout after
mediation. The dispute path is defined in [dispute.md](dispute.md).

All steps of this file need a deposit with `MIN_CONFIRMATIONS` confirmations
([setup-and-deposit.md](setup-and-deposit.md)).

## Messages

| Message | From | Content |
|---|---|---|
| E | Buyer | The buyer's partial signature for the swap transaction. It means: the payment is started. |
| F | Seller | The seller's secret key share of `P'`. It means: the payment is received. |
| G | Buyer | The buyer's secret key share of `Q'`. |

## Cooperative close

| Step | Trader | Precondition | The engine does | State after |
|---|---|---|---|---|
| 1. Start payment | Buyer | State `Deposit` | Hands out message E | `BuyerReadyToRelease` |
| 2. Receive message E | Seller | State `Deposit` | Checks the partial signature: it must complete the adaptor signature of the swap transaction. Stores it. | `BuyerReadyToRelease` |
| 3. Release | Seller | State `BuyerReadyToRelease`; enough time before the buyer's warning lock time ends (below) | Completes and stores the signed swap transaction. Hands out message F. | `SellerReadyToRelease` |
| 4. Receive message F | Buyer | State `BuyerReadyToRelease` | Checks that the key share matches the seller's public key share of `P'`. Computes the secret key of `P'`. Imports the payout into the wallet. Signs and stores the penalty transaction. Hands out message G. | `TradeClosed(Cooperative)` |
| 5. Receive message G | Seller | State `SellerReadyToRelease` | Checks that the key share matches the buyer's public key share of `Q'`. Computes the secret key of `Q'`. Imports the payout into the wallet. Signs and stores the penalty transaction. | `TradeClosed(Cooperative)` |

Result: the buyer owns `T + Db` and the seller owns `Ds`. Both amounts stay in the outputs of the
deposit transaction. No further transaction is needed.

Rules:

- The buyer's engine hands out message E only in state `BuyerReadyToRelease`. In every other
  state the response contains no partial signature for the swap transaction.
- The seller's engine hands out message F only after it holds the complete, valid swap
  transaction. The seller can then always finish the trade alone.
- A key share that does not match the public key share fails the validation
  ([trade-states.md](trade-states.md)).
- Only the buyer may perform step 1 and 4. Only the seller may perform step 2, 3 and 5.

### Release safety margin

The seller's engine refuses step 3 when fewer blocks than the release safety margin remain before
the buyer's warning transaction becomes valid. The buyer's warning transaction becomes valid when
the deposit has as many confirmations as the warning lock time.

Rationale: after message F the buyer holds the key of `P'`. If the buyer then publishes his
warning transaction, the seller cannot use the penalty, because he does not hold the key of `Q'`.
He can only redirect all funds to the burning men. The seller is safe against this only if his
swap transaction confirms before the buyer's warning transaction becomes valid. The margin is the
time reserved for that.

### After the close

The payouts stay in the deposit outputs until the trader spends them. Until then the peer could
still publish his warning transaction. The protection against this is defined in
[post-trade-protection.md](post-trade-protection.md).

## Forced close

The seller closes the trade alone when message G does not arrive.

| Step | Trader | Precondition | The engine does | State after |
|---|---|---|---|---|
| 1. Publish the swap transaction | Seller | State `SellerReadyToRelease` | Publishes the swap transaction | `TradeClosed(Forced)` |
| 2. See a spend of the seller's payout | Buyer | State `BuyerReadyToRelease` or `CustomPayoutSigned` | Reads the signature from the spending transaction. Computes the seller's key share of `P'` from it and checks it. Computes the secret key of `P'`. Imports the payout. | `TradeClosed(Forced)` |

Result: the seller owns `Ds` minus the fee of the swap transaction in his wallet. The buyer owns
`T + Db` in the deposit output.

Rules:

- The caller can ask for step 1 at any time in state `SellerReadyToRelease`.
- The engine performs step 1 on its own when the remaining number of blocks before the buyer's
  warning transaction becomes valid falls to the release safety margin. This rule is open point
  O8 in [README.md](README.md).
- The buyer does not know the swap transaction. His engine therefore examines every confirmed
  transaction that spends the seller's payout with the key path, not a transaction with a known
  ID.
- If message F arrives at the buyer after step 2, the buyer's trade is already closed. The
  message has no effect.
- The seller never gets the buyer's key share of `Q'` in a forced close. He needs none: his
  payout is in his wallet.

### Why the buyer can sign the swap transaction without seeing it

The buyer signs only a signature hash that the seller names. He cannot check which transaction
it belongs to. The reasons why this is safe for the buyer:

1. **The signature is valid for one output key only.** A Schnorr signature (BIP340) commits to
   the public key. The buyer creates his partial signature for the output key of the seller's
   payout. This key is `Q'` tweaked with the script of the payouts (BIP341). No other output of
   the trade has this key. In particular the escrow of the buyer's warning also has the internal
   key `Q'`, but a different script, and so a different output key. The seller cannot use the
   signature for the escrow.
2. **The only thing the signature can spend belongs to the seller.** The signature can authorise a
   spend of the seller's payout and nothing else. After the payment, the seller's payout is the
   seller's own money. Which transaction he uses to spend it, with which outputs and which fee,
   does not concern the buyer.
3. **The buyer hands the signature out only after he started the payment.** Before message E the
   seller holds no signature of the buyer for the key path of his payout, except the ones for the
   warning transactions. So the seller cannot take his deposit out of the trade before the buyer
   decided to pay.
4. **Every use of the signature reveals the seller's key share.** The buyer's partial signature is
   computed for a nonce that includes the adaptor point, the seller's public key share of `P'`.
   A complete signature that contains the buyer's partial signature therefore has exactly one
   possible value, and this value differs from the adaptor signature, which the buyer holds, by
   the seller's secret key share. This holds for every signature hash. Whatever the seller
   signed, the buyer learns the key of `P'` from the spend, and with it `T + Db`.
5. **One partial signature does not help to create another.** MuSig2 stays secure when the other
   signer chooses the message. The buyer's engine uses the nonce of the swap transaction for
   exactly one signature hash and never signs a second one with it.

What the buyer must do for this to hold:

- check, during the setup, that the adaptor signature is valid for the adaptor point that equals
  the seller's public key share of `P'` ([setup-and-deposit.md](setup-and-deposit.md));
- watch the seller's payout for any spend, not for one transaction ID;
- never sign two signature hashes with the nonce of the swap transaction.

What the seller risks: if he names a signature hash that belongs to no valid transaction, he has
no swap transaction. He then depends on message G or on the dispute path. This harms only him.

## Custom payout

A custom payout distributes the funds of the deposit as a mediator proposed. Both traders must
sign it. The mediation and the messages between the traders are defined by bisq2.

| Step | Trader | Precondition | The engine does | State after |
|---|---|---|---|---|
| 1. Sign | Each | State `Deposit` or `BuyerReadyToRelease` | Builds the custom payout transaction from the seller's amount and the fee rate of the request ([transactions.md](transactions.md)). Signs both inputs with the wallet's script key. | `CustomPayoutSigned` |
| 2. Finalise | Each | State `CustomPayoutSigned` | Checks that the peer's PSBT is for the same transaction and contains the peer's signatures. Completes the transaction. Publishes it. | `TradeClosed(Custom)` |

Rules:

- The request names the seller's amount before the fee and the fee rate. Both traders must use the
  same two values. The engine returns the transaction ID and the two output amounts, so that the
  caller can compare them with the mediation result.
- A trader who performed step 1 does not perform the steps of the cooperative close any more. His
  engine refuses to start the payment, to release, and to hand out a key share.
- The seller cannot perform step 1 in `SellerReadyToRelease`. He released the buyer's payout
  already and closes with message G or with the swap transaction.
- The buyer can perform step 1 after he received nothing from the seller, also when the seller
  has released already. If the seller's swap transaction then confirms, the buyer's trade closes
  as a forced close.
- A second request for step 1 with other values fails the validation. The engine never signs two
  different custom payout transactions for one trade.
- A PSBT of the peer for another transaction, with other outputs or other amounts, fails the
  validation.
- Step 2 can be repeated. It returns the same transaction and sends it again.
- A successful step 2 means that the transaction is complete and was sent. It does not mean that
  it is confirmed. The trade is final when the transaction has `MIN_CONFIRMATIONS` confirmations.
- If only one trader signs, there is no valid transaction. The trade stays in
  `CustomPayoutSigned`. The engine sets no time limit for the peer. The trader can leave this
  state through the dispute path.

The custom payout transaction and the swap transaction both spend the seller's payout, so only one
of them can confirm. The engine follows the one that confirms
([chain-observation.md](chain-observation.md)).
