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
| `DepositAborted` | The deposit transaction can never confirm, because a confirmed transaction spends one of its inputs: the deposit abort of a trader after the deposit timeout, or another spend of a coin ([setup-and-deposit.md](setup-and-deposit.md)). |
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

## State changes that follow from the chain

A state change has one of two causes: a request of the caller, or a confirmed transaction. The
engine treats them differently.

- A request changes the state as the table above allows, and the change stays. The same holds for
  the two steps that the engine takes on its own in place of a request: the forced close of the
  seller at the release safety margin, and the close of the buyer when he has read the seller's
  key share from a confirmed transaction ([closing.md](closing.md)).
- The state that follows from confirmed transactions is derived from the chain every time the
  engine looks at it: the warning states for the trader who did not ask for the warning, the
  closure types of redirect, claim and penalty, `Custom` for the trader who did not finalise,
  `Forced` for the seller whose swap transaction confirmed without his request, and
  `DepositAborted`.

Consequences:

- When a reorganisation removes the transaction, the derived state goes back to the state that
  the requests caused. The trade continues from there ([chain-observation.md](chain-observation.md)).
- A derived state can follow a state that the table does not connect with it, because the chain
  decides what happened and not the intention of the trader:
  - a trader who decided to publish his warning transaction is in the warning state of the peer
    when the peer's warning transaction confirms instead;
  - a seller who closed by force, or a trader who finalised a custom payout, is in the warning
    state of the peer when the peer's warning transaction confirms before his own transaction.
    His own transaction can then never confirm.
- The requests see the derived state. For example, a trader cannot start the payment after the
  warning transaction of the peer confirmed.

The project owner accepted this section on 2026-11-18 (D22 in `concept/CompletionPlan.md`).

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

Rationale: the caller cannot know whether a response was lost. It must be able to repeat a request
safely. A second signature with the same nonce on other data would reveal a key.

## Failed checks: repeat or terminate

A failed check has one of two consequences. The question that decides is: can this failure happen
although the peer runs our unmodified software?

### The failure proves a modified or foreign client: the trade is terminated

The engine terminates the trade when it receives data of the peer that our software never
produces, whatever happens on the network. These are:

- a field of a message of the peer that cannot be decoded: no valid point, nonce, signature,
  transaction, PSBT or address;
- a field or a list of a message of the peer above its size limit;
- key shares or a script key of the peer that break the rules of
  [setup-and-deposit.md](setup-and-deposit.md);
- a half of the deposit that breaks the rules of that file;
- an address of the peer for another network;
- a nonce of the peer that is used for more than one signature;
- a partial signature of the peer that is not valid for the transaction that the trader computed.
  Different lock times in the configuration of two unmodified clients have the same effect, and
  the engine cannot tell the two cases apart ([parameters.md](parameters.md));
- a partial signature in message E that does not complete the adaptor signature of the swap
  transaction;
- a secret key share in message F or G that does not match the public key share of the peer;
- a custom payout PSBT of the peer for the trader's own transaction with an invalid signature;
- data of the peer that differs from data of the same kind that the trade already holds, for
  example a second message with other nonces.

When the engine terminates a trade:

- It marks the trade as terminated and stores the mark with the reason.
- It writes a log entry at error level with the details: trade ID, role, state, the request, the
  check that failed, the field, and the value that failed. A secret is never logged. For a wrong
  secret key share the entry names the public key share that it should match.
- It reports the termination and the reason to the caller.
- It accepts no further data for this trade, also no corrected data.
- It does not continue the cooperative steps of the trade. It hands out no further signature and
  no key share.

There is no recovery from this mark.

Rationale: such data comes from a peer whose software was changed, or from other software. This
peer may be preparing an attack. The engine stops cooperating at the first proof, so that it
cannot be used to test prepared data. The detailed log entry lets the user and the developers see
what the peer did.

### The failure can have another cause: the request is refused and can be repeated

In every other case the engine refuses the request. The trade is not changed and continues. The
caller can send the request again. These cases are:

- a precondition does not hold: the request came too early or in the wrong order, for the wrong
  role, or the deposit has too few confirmations;
- data that the caller itself supplied is invalid: trade parameters, redirection receivers, fee
  rates, the amount of a custom payout;
- a condition outside of the trade: the wallet is closed, no peer of the network is reachable;
- a custom payout PSBT of the peer for another transaction than the trader's own. The two callers
  can have used different amounts or fee rates.

Messages of the peer can be lost, delayed, delivered twice or delivered in another order. None of
this terminates a trade: a message that comes too early is refused by a precondition, and a
message that comes twice is a repeated request.

If the engine cannot tell whether a failure proves a modified client, it refuses the request and
does not terminate the trade.

### A failed check during the setup

During the setup (`Init`) a partial signature of the peer is invalid also when the two callers
passed different trade parameters to their engines. The engine cannot tell this case from a
modified client. It terminates the trade in both cases, and the log entry says that both causes
are possible. No funds are bound in `Init`, so the only consequence is that the traders must start
a new trade with a new trade ID.

## Terminated trades

A trade that is terminated in `Init` is abandoned.

A trade that is terminated after the trader signed the deposit still has funds. The decision
which actions such a trade keeps is postponed (open point O1 in [README.md](README.md)). Until it
is decided, this rule applies:

The engine keeps watching the chain for a terminated trade, and it keeps every action that the
trader can perform alone with the data that the trade already holds:

- publishing the trader's own warning transaction, when its lock time has passed;
- the reactions to a warning transaction of the peer (redirect, penalty) and the claim;
- for the seller, publishing the swap transaction, if he holds the buyer's partial signature;
- for the buyer, reading the seller's key share from a spend of the seller's payout;
- the deposit abort.

Rationale: these actions do not depend on the peer, and without them the funds of the trader
would stay in the deposit for ever.
