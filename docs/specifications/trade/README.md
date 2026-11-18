# Specification of the Bisq MuSig trade protocol

These files are the authoritative description of what the trade protocol must do. They take
precedence over behaviour that can be read from the code or from the tests. The folder `concept/`
keeps the history of the design and is not updated.

The specification describes the target. Parts of it are not implemented yet.
`concept/CompletionPlan.md` lists what exists and the work that is left.

## Files

| File | Content |
|---|---|
| [trade-states.md](trade-states.md) | States of a trade, allowed state changes, order of checks for a request, repeated requests, terminated trades |
| [transactions.md](transactions.md) | Every transaction of a trade: inputs, outputs, scripts, signatures, constants |
| [parameters.md](parameters.md) | Trade parameters, constants, configuration values, limits, minimum amounts |
| [key-derivation.md](key-derivation.md) | Derivation of the key shares from the seed, trade IDs, nonces, test vectors |
| [setup-and-deposit.md](setup-and-deposit.md) | Setup messages, checks of the peer's data, deposit, abort |
| [closing.md](closing.md) | Cooperative close, forced close with the swap transaction, custom payout |
| [dispute.md](dispute.md) | Warning, redirect, claim by script, penalty |
| [chain-observation.md](chain-observation.md) | Access to the chain, confirmations, sending, reorganisations, reaction to every chain event |
| [post-trade-protection.md](post-trade-protection.md) | Protection of a payout after a cooperative close; drain when the program ends |
| [fees.md](fees.md) | Fee rates, who pays, raising a fee |
| [persistence-and-recovery.md](persistence-and-recovery.md) | Trade store, restart, backup and restore |
| [service-interface.md](service-interface.md) | Meaning of the requests of bisq2, errors, security assumptions |

[key-derivation-vectors.py](key-derivation-vectors.py) computes the test vectors of
`key-derivation.md`.

Reading order for a first reading: `transactions.md`, `trade-states.md`, `setup-and-deposit.md`,
`closing.md`, `dispute.md`, then the rest.

## Terms

| Term | Meaning |
|---|---|
| Buyer, seller | The buyer buys bitcoin and pays in another currency. The seller sells bitcoin. |
| Maker, taker | The maker published the offer. The taker took it. |
| Trader, peer | "The trader" is the user whose engine is described. "The peer" is the other one. |
| Engine | The part of `musigd` that runs the trade protocol and holds the keys |
| Caller | The bisq2 application of the trader |
| Payout | One of the two outputs of the deposit transaction that hold the funds of the trade |
| Escrow | The output of a warning transaction that holds the funds of the trade |
| Confirmed | Included in a block of the chain with the most work. "n confirmations" means that the block and n - 1 blocks after it exist. |

A rule that uses "must", "never" or the present tense ("the engine refuses") is a requirement.
A paragraph that starts with "Rationale" explains a requirement and is not one itself.

## Where the decisions are specified

The decisions D1 to D23 of `concept/CompletionPlan.md`, section 7:

| Decision | Specified in |
|---|---|
| D2 Chain access through compact block filters only | [chain-observation.md](chain-observation.md) |
| D3 Claim by script | [transactions.md](transactions.md), [dispute.md](dispute.md) |
| D4 Redirect lock time 1 block | [transactions.md](transactions.md), [fees.md](fees.md) |
| D5 Amounts and fee rates as trade parameters | [parameters.md](parameters.md) |
| D6 Three confirmations of the deposit | [parameters.md](parameters.md), [setup-and-deposit.md](setup-and-deposit.md), [chain-observation.md](chain-observation.md) |
| D7 The engine starts time-critical reactions | [chain-observation.md](chain-observation.md), [dispute.md](dispute.md) |
| D8 Protection after the close, drain | [post-trade-protection.md](post-trade-protection.md) |
| D9 One trade fee output in the deposit | [transactions.md](transactions.md), [parameters.md](parameters.md) |
| D10 Key shares from the seed | [key-derivation.md](key-derivation.md) |
| D11 No authentication of the service | [service-interface.md](service-interface.md) |
| D12 No check of the peer's deposit inputs on chain | [setup-and-deposit.md](setup-and-deposit.md) |
| D13 Deposit abort | [setup-and-deposit.md](setup-and-deposit.md), [transactions.md](transactions.md) |
| D14 Minimum amounts from dust limit and fee rate | [parameters.md](parameters.md) |
| D17 Proof of a modified or foreign client of the peer terminates the trade; other failures can be repeated | [trade-states.md](trade-states.md) |
| D18 Validate before storing, check the state before any change | [trade-states.md](trade-states.md) |
| D19 Lock times are configuration values of the engine, read from the environment; they are not part of the service interface | [parameters.md](parameters.md) |
| D20 The engine connects only to the configured peers | [chain-observation.md](chain-observation.md) |
| D21 During a problem with the peers a request that only observes the chain is accepted; a request that must send is refused | [chain-observation.md](chain-observation.md), [service-interface.md](service-interface.md) |
| D22 The state that follows from confirmed transactions is derived from the chain | [trade-states.md](trade-states.md), [chain-observation.md](chain-observation.md) |
| D23 The seller's engine publishes the swap transaction on its own at the forced close margin, a configuration value | [closing.md](closing.md), [parameters.md](parameters.md) |

D1, D15 and D16 are decisions about the code and the documents. They need no rule here.

## Open points

Each of these points is a rule that this specification states, but that the project owner has not
confirmed. The text of the files contains the rule of the column "Rule in the specification".

| # | Question | Rule in the specification | File |
|---|---|---|---|
| O1 | What may a terminated trade (D17) still do after the deposit is signed? **Postponed** on 2026-11-18; listed in `concept/CompletionPlan.md`, section 12, row 8. | Until the decision: the cooperative steps end. The engine keeps watching the chain and keeps every action that the trader can perform alone: warning, redirect, penalty, claim, swap transaction, reading the key from a spend, deposit abort. | [trade-states.md](trade-states.md) |
| O3 | How does the engine handle a broadcast without a result (N2)? | Status "sent", repeated sending after every block until the transaction or a conflicting one confirms. A sending that fails is repeated after some seconds. | [chain-observation.md](chain-observation.md) |
| O4 | Peers and the visible IP address (N4) | The peer list is a start option. Hiding the IP address is not specified. | [chain-observation.md](chain-observation.md) |
| O5 | Wording of the security assumption for the open port (N7) | The computer and all its local users are trusted. | [service-interface.md](service-interface.md) |
| O7 | Default values of the lock times, the deposit timeout and the release safety margin; limits of the lock times (N9) | Defaults until the decision: warning lock time 1440 blocks, claim lock time 720 blocks, deposit timeout 144 blocks, release safety margin 6 blocks. The limits are: warning lock time larger than 3 plus the release safety margin; claim lock time larger than 1. | [parameters.md](parameters.md) |
| O9 | Values of the fee schedule: blocks between two steps, size of a step, upper limit | Implemented with these values until the decision: a step multiplies the fee rate by 3/2; at most 10 steps; 6 blocks per step for a transaction without a deadline; for a transaction with a deadline the time that it has, divided by 20. A child pays at most the amount that its transaction spends. | [fees.md](fees.md) |
| O10 | Size limits of message fields and lists | No values are specified. | [parameters.md](parameters.md) |
| O11 | gRPC status codes and error details for the error cases | Five cases are specified. The status codes in the file are the ones that the service uses until the decision. | [service-interface.md](service-interface.md) |
| O12 | Sanity limits of the engine for the deposit fee rate and the prepared fee rate | Implemented with these values until the decision: at least 1 sat per vbyte, at most 1000 sat per vbyte. | [fees.md](fees.md) |
| O13 | Which request carries the trade parameters (amounts, fee rates, trade fee receiver)? The first version of this specification named the request that creates the trade (step 1). | Step 2 (`GetNonceShares`), as in the existing messages. No message changes. Nothing in step 1 depends on the parameters. | [parameters.md](parameters.md), [setup-and-deposit.md](setup-and-deposit.md) |
| O14 | Two unmodified clients with different lock times in their configuration (D19): the partial signatures do not fit, and the engine cannot tell this from a modified client. | The setup is terminated (D17). Nothing is signed, so no funds are at risk. The reason names the trader's own lock times. | [parameters.md](parameters.md), [trade-states.md](trade-states.md) |
| O18 | Which deadlines and which actions of the user do the events of a trade report, and in which form? The plan names "deadlines as block heights" and "required user actions" without a list. | Five deadlines and three actions, each defined by a rule of the engine. Every event carries the current deadlines, actions and the reason of a termination. The form is not agreed with bisq2. | [service-interface.md](service-interface.md) |
| O19 | Details of the deposit abort (D13): (a) the engine aborts on its own only in the state `Deposit`, and in a later state it sends the deposit again; (b) the deposit timeout is counted again after a reorganisation removed the deposit; (c) the abort pays more than the fee of the whole deposit transaction, so that it replaces the deposit in a node; (d) the abort pays to the trader's payout address; (e) the abort is not stored. | As listed in the question. | [setup-and-deposit.md](setup-and-deposit.md), [transactions.md](transactions.md) |
| O20 | Which stored transactions are sent again after a start (N5)? The plan names the drain transaction. | Every unconfirmed transaction that spends coins of the wallet: the drain, wallet payments and recorded transactions of trades. They are sent after the first sync of a run and after every new block, until they are confirmed or can no longer confirm. | [post-trade-protection.md](post-trade-protection.md), [../wallet/coin-reservation.md](../wallet/coin-reservation.md) |
| O17 | A record of the trade store cannot be loaded. The engine then serves no trade ([persistence-and-recovery.md](persistence-and-recovery.md)). How does the user leave this condition? | Not specified. No request removes or repairs a record. Until the decision the user needs a copy of the wallet database from before the damage. | [persistence-and-recovery.md](persistence-and-recovery.md) |

Points that were open and are decided:

| # | Question | Decision of the project owner | File |
|---|---|---|---|
| O2 | Which failed checks terminate a trade (D17)? | 2026-11-18: everything by which the engine detects that the peer tampered with the software or does not run our client terminates the trade, and the reason is logged in detail. Conditions that can have another cause, for example a network problem, can be repeated. | [trade-states.md](trade-states.md) |
| O6 | Path and tag of the key derivation (N6) | 2026-11-18: accepted as specified. Path `m/86'/coin_type'/0'/2'`, tag `BisqMuSig/key-share`. | [key-derivation.md](key-derivation.md) |
| O8 | When does the seller's engine publish the swap transaction on its own (D7)? | 2026-11-18: when the number of blocks before the buyer's warning becomes valid falls to the forced close margin (D23). The margin is a configuration value. Its default is the release safety margin, which gives about ten days with the default lock time. The caller can ask for the swap transaction earlier. | [closing.md](closing.md), [parameters.md](parameters.md) |
| O15 | Does the engine use only the peers that the user configured? The library for compact block filters looks for other peers through DNS seeds when the configured peers fail. | 2026-11-18: yes, configured peers only (D20). The reason given: the software operates only on regtest at present. | [chain-observation.md](chain-observation.md) |
| O16 | Which state changes follow from the chain, and do they go back after a reorganisation? | 2026-11-18: accepted as specified (D22). The state that follows from confirmed transactions is derived from the chain every time and goes back when the transaction is removed. A derived state can follow a state that the table of state changes does not connect with it. | [trade-states.md](trade-states.md) |

## Differences to the state model in the code

- `protocol/src/state.rs` holds the table of the state changes that requests cause. The state
  changes that follow from the chain are not in this table: the engine derives them from the
  confirmed transactions (`TradeModel::observe_chain`), as `trade-states.md` describes. This
  covers the change from `SellerReadyToRelease` to `BuyersWarning` for the seller.
- The terminated mark is a field of the trade, with the reason. It is stored with the trade
  ([persistence-and-recovery.md](persistence-and-recovery.md)).
