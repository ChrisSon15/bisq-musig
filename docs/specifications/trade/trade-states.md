# Trade states and request handling

This file defines the states of a trade, the allowed state changes for each role, and the rules
that apply to every request. The other files refer to the state names used here.

## Roles

Every trade has a buyer and a seller of bitcoin. One of them is the maker of the offer, the other
is the taker. The role of a trader is one of `BuyerAsMaker`, `BuyerAsTaker`, `SellerAsMaker`,
`SellerAsTaker`. It is fixed when the trade is created.

Maker and taker differ only in the order of the setup messages
([setup-and-deposit.md](setup-and-deposit.md)). All other rules depend only on buyer or seller.

## States

Each trader keeps his own state of the trade. The two traders are often in different states.

| State | Meaning for the buyer | Meaning for the seller |
|---|---|---|
| `Init` | Setup data is exchanged. Nothing is signed that binds funds. The trade can be abandoned. | Same |
| `Deposit` | The trader signed his inputs of the deposit transaction. The deposit may be unpublished, unconfirmed or confirmed. | Same |
| `BuyerReadyToRelease` | The buyer started the payment and is ready to hand out his partial signature for the swap transaction. | The seller holds the buyer's partial signature for the swap transaction. |
| `SellerReadyToRelease` | Not used | The seller confirmed the payment and is ready to hand out his key share for the buyer's payout. |
| `CustomPayoutSigned` | The trader signed a custom payout transaction. He does not close the trade in the normal way any more. | Same |
| `BuyersWarning` | The buyer decided to publish his warning transaction. | The buyer's warning transaction is confirmed. |
| `SellersWarning` | The seller's warning transaction is confirmed. | The seller decided to publish his warning transaction. |
| `TradeClosed(c)` | The trade ended with closure type `c`. | Same |

Closure types:

| Closure type | Outcome |
|---|---|
| `Cooperative` | The key shares were exchanged. No transaction was needed. |
| `Forced` | The seller's payout was spent by the swap transaction. |
| `Custom` | The custom payout transaction is the closing transaction. |
| `BuyersRedirect`, `SellersRedirect` | The named trader published the redirect transaction after the warning transaction of the peer. |
| `BuyersClaim`, `SellersClaim` | The named trader claimed the escrow of his own warning transaction. |
| `BuyersPenaltyTx`, `SellersPenaltyTx` | The named trader took the escrow of the peer's warning transaction with the penalty transaction. |
| `DepositAborted` | The trader spent his own deposit input after the deposit timeout ([setup-and-deposit.md](setup-and-deposit.md)). |
| `Abandoned` | The trade ended in `Init`. No funds were bound. |

## Allowed state changes

A state change is allowed only if the table lists it for the role of the trader. "Both" means buyer
and seller.

| From | To | Role |
|---|---|---|
| `Init` | `Deposit` | Both |
| `Init` | `TradeClosed(Abandoned)` | Both |
| `Deposit` | `BuyerReadyToRelease` | Both |
| `Deposit` | `TradeClosed(DepositAborted)` | Both |
| `Deposit`, `BuyerReadyToRelease` | `CustomPayoutSigned` | Both |
| `Deposit`, `BuyerReadyToRelease`, `CustomPayoutSigned` | `BuyersWarning` | Both |
| `Deposit`, `BuyerReadyToRelease`, `CustomPayoutSigned` | `SellersWarning` | Both |
| `BuyerReadyToRelease` | `SellerReadyToRelease` | Seller |
| `BuyerReadyToRelease` | `TradeClosed(Cooperative)` | Buyer |
| `BuyerReadyToRelease`, `CustomPayoutSigned` | `TradeClosed(Forced)` | Buyer |
| `SellerReadyToRelease` | `TradeClosed(Cooperative)`, `TradeClosed(Forced)` | Seller |
| `SellerReadyToRelease` | `BuyersWarning` | Seller |
| `CustomPayoutSigned` | `TradeClosed(Custom)` | Both |
| `BuyersWarning` | `TradeClosed(SellersRedirect)`, `TradeClosed(BuyersClaim)` | Both |
| `SellersWarning` | `TradeClosed(BuyersRedirect)`, `TradeClosed(SellersClaim)` | Both |
| `TradeClosed(Cooperative)` | `TradeClosed(BuyersPenaltyTx)`, `TradeClosed(SellersClaim)` | Buyer |
| `TradeClosed(Cooperative)` | `TradeClosed(SellersPenaltyTx)`, `TradeClosed(BuyersClaim)` | Seller |

Notes:

- The seller cannot go from `BuyerReadyToRelease` directly to a closed state. He must confirm the
  payment first (`SellerReadyToRelease`).
- The seller cannot sign a custom payout in `SellerReadyToRelease`, and cannot publish his own
  warning transaction in that state. He already released his key share, so both actions could
  only harm him.
- `SellerReadyToRelease` to `BuyersWarning` covers the case that the buyer publishes his warning
  transaction instead of answering the release ([dispute.md](dispute.md)).
- A change from `TradeClosed(Cooperative)` to a claim closure type records a loss: the peer
  published his warning transaction after the close and the trader did not react in time.
- A trade never leaves a closed state, except for the changes in the last two rows.

A chain reorganisation can undo the event that caused a state change. The rules for this are in
[chain-observation.md](chain-observation.md).

## Order of checks for a request

Requests come from the local caller (bisq2). Many of them carry data that the peer sent. The
engine processes every request for an existing trade in this order:

1. **Preconditions.** The trade exists, the request is allowed for the role and the state of the
   trade, and the chain conditions of the request hold (for example the number of confirmations
   of the deposit).
2. **Validation.** All data of the request is decoded and checked, against the rules of this
   specification and against the data that the trade already holds.
3. **Change.** Only now the engine stores data, signs, or changes the state.

A request that fails in step 1 or 2 leaves no data and no signature in the trade.

Rationale: a signature or a stored value that exists although the request was refused is a state
that no rule of this specification describes. It can be used later through another request.

## Repeated requests

A request that is repeated with the same data returns the same result as the first time. It does
not sign again and does not change the state. This holds also when the trade has moved to a later
state, as long as the engine still holds the data of the result.

A request that carries other data than the stored data of an earlier request fails the validation.

Rationale: the caller cannot know whether a response was lost. It must be able to repeat a request
safely. A second signature with the same nonce on other data would reveal a key.

## Terminated trades

A request that fails the validation terminates the trade:

- The engine marks the trade as terminated. The mark is stored with the trade.
- The engine accepts no further request data for this trade, also no corrected data.
- The engine does not continue the cooperative steps of the trade. It hands out no further
  signature and no key share.
- The engine reports the termination to the caller.

There is no recovery from this mark.

A request that is refused by a precondition (step 1) does not terminate the trade. The caller can
send the request again when the precondition holds.

A terminated trade still has funds if the deposit is signed. For this reason the engine keeps
watching the chain for a terminated trade, and it keeps every action that the trader can perform
alone with the data that the trade already holds:

- publishing the trader's own warning transaction, when its lock time has passed;
- the reactions to a warning transaction of the peer (redirect, penalty) and the claim;
- for the seller, publishing the swap transaction, if he holds the buyer's partial signature;
- for the buyer, reading the seller's key share from a spend of the seller's payout;
- the deposit abort.

A trade that is terminated in `Init` is abandoned.

Rationale: invalid data comes from a peer or a caller that does not follow the protocol. The
trader's engine stops cooperating at the first such event, so that it cannot be used as a test
service for prepared data. The actions that the trader performs alone do not depend on the peer
and are needed to get the funds out of the deposit.

Open points O1 and O2 in [README.md](README.md) belong to this section.
