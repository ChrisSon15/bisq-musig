# Transactions of a trade

This file defines every transaction of the trade protocol: what it spends, what it pays, who can
publish it, and how it is signed. The values of the parameters are defined in
[parameters.md](parameters.md).

## Names

| Name | Meaning |
|---|---|
| `T` | Trade amount |
| `Db`, `Ds` | Security deposit of the buyer, of the seller |
| `P'` | Aggregated key of the buyer's payout |
| `Q'` | Aggregated key of the seller's payout |
| Key share | The part of an aggregated key that one trader holds. Each trader holds one key share of `P'` and one of `Q'`. |
| Script key | A key of a trader's wallet that is used in the script of the payouts |
| Prepared fee rate | The fee rate of the pre-signed transactions, agreed at trade start |

All amounts are in satoshi. All transactions have version 2 and lock time 0. Fees are rounded up
to the next satoshi.

## Keys

`P'` and `Q'` are MuSig2 aggregated keys (BIP327) of the two key shares of the traders. The two
public key shares are sorted by their compressed encoding before they are aggregated.

The key shares are derived from the wallet seed ([key-derivation.md](key-derivation.md)).

Who gives away which key share:

- In a cooperative close the seller gives his key share of `P'` to the buyer, and the buyer gives
  his key share of `Q'` to the seller.
- The buyer's key share of `P'` and the seller's key share of `Q'` are never given away. They are
  the claim keys of the warning escrows.

## Outputs with scripts

All outputs that the protocol creates for itself are Taproot outputs (BIP341).

| Output | Internal key | Script leaf (the only one) |
|---|---|---|
| Buyer's payout (in the deposit) | `P'` | `<buyer's script key> CHECKSIGVERIFY <seller's script key> CHECKSIG` |
| Seller's payout (in the deposit) | `Q'` | The same script |
| Escrow of the buyer's warning | `Q'` | `<buyer's key share of P'> CHECKSIGVERIFY <claim lock time> CHECKSEQUENCEVERIFY` |
| Escrow of the seller's warning | `P'` | `<seller's key share of Q'> CHECKSIGVERIFY <claim lock time> CHECKSEQUENCEVERIFY` |

Keys in scripts are x-only public keys.

Consequences of this design:

- After a cooperative close the buyer knows both key shares of `P'`. He can spend his payout with
  the key path. He can also spend the escrow of the seller's warning with the key path. This is
  the penalty.
- The seller has the same position for `Q'`, his payout and the escrow of the buyer's warning.
- The script leaf of the payouts is used only by the custom payout.
- The script leaf of an escrow is used only by the claim.
- The payout of the seller and the escrow of the buyer's warning have the same internal key but
  different script leaves. Their output keys are therefore different. The security of the swap
  transaction depends on this ([closing.md](closing.md)).

## Overview

| Transaction | Spends | Pays to | Published by | Signed |
|---|---|---|---|---|
| Deposit | Wallet coins of both traders | Both payouts, the trade fee, change | Taker | Each wallet signs its own inputs |
| Swap | Seller's payout | Seller's wallet | Seller | MuSig2 adaptor signature, at setup and payment start |
| Warning of a trader | Both payouts | Escrow, anchor of this trader | This trader | MuSig2, at setup |
| Redirect of a trader | Escrow of the **peer's** warning | Redirection receivers, anchor of this trader | This trader | MuSig2, at setup |
| Claim of a trader | Escrow of his **own** warning | His wallet | This trader | Alone, with his claim key, when needed |
| Penalty of a trader | Escrow of the **peer's** warning | His wallet | This trader | Alone, with the full aggregated key, at close |
| Custom payout | Both payouts | Both wallets | Either trader | Both wallets, with the script keys |
| Deposit abort | The trader's own deposit inputs | His wallet | This trader | His wallet |
| Drain | Imported payouts of closed trades | The trader's seed wallet | This trader | His wallet |

"At setup" means: the signatures are exchanged before the deposit transaction is signed. Each
trader holds the full signatures only for his own warning and redirect transaction.

## Deposit transaction

Inputs: coins of the buyer's wallet and coins of the seller's wallet. Every input has the sequence
number `0xFFFFFFFD`. Every input spends a P2TR or a P2WPKH output.

Outputs:

| Output | Amount | Paid by |
|---|---|---|
| Buyer's payout | `T + Db` | Buyer `Db`, seller `T` |
| Seller's payout | `Ds` | Seller |
| Trade fee (at most one) | As agreed | Seller |
| Change of each trader (optional) | Rest | - |

Each trader pays the mining fee for his own inputs and outputs and for half of the fixed part of
the transaction, at the deposit fee rate.

The order of inputs and outputs is a shuffle that both traders compute in the same way from data
that only they know. An observer cannot tell from the order which output is which.

The rules for building and checking the deposit are in
[setup-and-deposit.md](setup-and-deposit.md).

## Swap transaction

- Input: the seller's payout, sequence number `0xFFFFFFFD`.
- Output: `Ds` minus the fee, to an address of the seller's wallet.
- Fee: prepared fee rate times 444 weight units.
- Signature: key path, with a MuSig2 adaptor signature. The adaptor point is the seller's public
  key share of `P'`. The completed signature therefore reveals the seller's key share of `P'` to
  the buyer.

Only the seller knows the transaction. The buyer signs only its signature hash
([closing.md](closing.md)).

## Warning transactions

There is one warning transaction for each trader. Both spend the same two outputs, so only one of
them can confirm.

- Inputs: the buyer's payout and the seller's payout, in this order. Both have a relative lock
  time of the warning lock time (BIP68, in blocks).
- Output 0, escrow: `T + Db + Ds` minus the anchor amount minus the fee.
- Output 1, anchor: the anchor amount, to an address of the trader who can publish the
  transaction.
- Fee: prepared fee rate times 846 weight units.
- Signatures: key path for both inputs, MuSig2.

## Redirect transactions

The redirect transaction of a trader spends the escrow of the peer's warning transaction.

- Input: the escrow, with a relative lock time of 1 block.
- Outputs: one output for each redirection receiver, then an anchor output with the anchor amount
  for the trader who can publish the transaction.
- Fee: the escrow amount minus all outputs. The receivers are chosen at setup so that the fee
  matches the prepared fee rate ([parameters.md](parameters.md)).
- Signature: key path, MuSig2.

The redirection receivers are the burning men of the Bisq DAO. The traders get nothing from this
transaction. A DAO arbitrator decides about a reimbursement outside of this protocol.

Rationale for the lock time of 1 block: the redirect transaction cannot enter the mempool before
the warning transaction is confirmed. The only unconfirmed child that a warning transaction can
have is then a transaction that spends its anchor, and only the publisher of the warning can
create it. The peer cannot block a fee increase of the warning transaction.

## Claim transaction

The claim transaction of a trader spends the escrow of his own warning transaction.

- Input: the escrow, with a relative lock time of the claim lock time.
- Output: the rest after the fee, to an address of the trader's wallet.
- Fee: the engine's own fee rate at the time of the claim ([fees.md](fees.md)).
- Signature: script path, with the trader's claim key.

The claim transaction is not pre-signed. The trader builds and signs it alone when he needs it,
and he can replace it with a higher fee.

Rationale: a pre-signed claim would need a fee rate that is fixed at trade start, and an anchor
output to raise it later. The claim key is never given to the peer, so the trader does not need
the peer for this signature.

## Penalty transaction

The penalty transaction of a trader spends the escrow of the peer's warning transaction with the
key path. The trader can sign it alone when he holds both key shares of the internal key of that
escrow, that is after a cooperative close.

- Input: the escrow, sequence number `0xFFFFFFFD`.
- Output: the escrow amount minus the fee, to the payout address that the trader named at setup.
- Fee: the penalty fee rate times 444 weight units. The penalty fee rate is the prepared fee rate
  unless the trader sets another one at close.

The trader builds and signs the penalty transaction when he closes the trade, and keeps it.

## Custom payout transaction

- Inputs: the buyer's payout and the seller's payout, in this order, sequence number
  `0xFFFFFFFD`. Both are spent with the script path. Each input needs one signature of each
  trader's script key.
- Output 0: the buyer's amount, to the payout address that the buyer named at setup.
- Output 1: the seller's amount, to the payout address that the seller named at setup.
- Fee: the custom payout fee rate times 1182 weight units, if both payout addresses are P2TR
  addresses. For other address types the weight changes by the difference of the script sizes.

Amounts: the request names the seller's amount before the fee, `S`. With the fee `F`:

- seller's output = `S` minus `F / 2`, rounded down;
- buyer's output = `T + Db + Ds` minus `S` minus the rest of the fee.

If `F` is odd, the buyer pays 1 satoshi more than the seller.

## Deposit abort transaction

A trader who signed the deposit and does not see it confirmed within the deposit timeout spends
his own deposit inputs to his own wallet. When this transaction is confirmed, the deposit
transaction can no longer confirm.

- Inputs: all coins that the trader put into the deposit transaction, with the sequence number
  `0xFFFFFFFD`.
- Output: one output to the trader's payout address. No other transaction of the trade can pay to
  this address once the deposit is aborted, so the address is used once.
- Fee: at least the engine fee rate for its own weight ([fees.md](fees.md)), and more than the
  fee of the whole deposit transaction, by 1 sat per vbyte of its own weight.

Rationale for the fee: a node can hold the deposit transaction as unconfirmed, and the trader
cannot see that. The abort must then replace the deposit in this node. A node accepts a
replacement only if it pays more than the transaction that it replaces, by the incremental relay
fee for its own weight, and at a higher fee rate. The abort is smaller than the deposit, so the
higher fee gives the higher fee rate. A fee above the trader's own part of the deposit fee would
not be enough.

## Drain transaction

The drain transaction moves all payouts of closed trades, which the wallet holds as imported
coins, to an address of the seed wallet ([post-trade-protection.md](post-trade-protection.md)).

## Constants

| Constant | Value |
|---|---|
| Anchor amount | 330 sat |
| Weight of a signed warning transaction | 846 wu |
| Weight of a signed swap or penalty transaction | 444 wu |
| Fixed part of the weight of a signed redirect transaction | 444 wu |
| Weight of a signed custom payout transaction with two P2TR outputs | 1182 wu |
| Redirect lock time | 1 block |
