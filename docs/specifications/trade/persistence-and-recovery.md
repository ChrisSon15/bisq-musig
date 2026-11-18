# Persistence and recovery

This file defines what the engine stores about a trade, when it stores it, and what happens after
a restart or a restore.

## The trade store

The engine keeps all trades in one durable, encrypted store. The store is protected in the same
way as the wallet database.

Stored for each trade:

- trade ID, role, trade parameters, state, and the terminated mark;
- the peer's public key shares, script key, addresses and half of the deposit;
- the deposit transaction; the trader's own fully signed warning and redirect transaction;
- for the buyer, the adaptor signature of the swap transaction; for the seller, the swap
  transaction and, once received, the buyer's partial signature for it;
- the peer's secret key share, once received, and the penalty transaction;
- the custom payout PSBTs of the trader and of the peer, and the final custom payout transaction;
- the trader's reserved coins;
- every transaction that the engine sent, with its status;
- the height at which the trader signed the deposit.

Stored once for the wallet:

- the set of all trade IDs that were ever used, including closed and abandoned trades;
- the drain transaction, while it is unconfirmed
  ([post-trade-protection.md](post-trade-protection.md)).

Never stored:

- the trader's own secret key shares. They are derived from the seed when they are needed
  ([key-derivation.md](key-derivation.md));
- secret nonces.

## When the engine stores

**The new state is in the store before any signature, key share or signed transaction leaves the
engine.**

This holds for every response to the caller and for every transaction that the engine sends.

Rationale: if the engine handed out a signature and then forgot that it did, it could sign other
data in the same situation after a restart. And if it handed out its signatures for the deposit
without having stored its warning transaction, it could not leave the deposit alone.

## Repeated requests after a restart

The caller cannot know whether a response was lost when the process stopped. Every request can
therefore be repeated after a restart with the same data and returns the same response
([trade-states.md](trade-states.md)).

If the process stopped before the state was stored, no signature and no secret left the engine.
The repeated request is then processed as a new request.

## Restart

At start-up the engine:

1. loads all trades from the store;
2. abandons every trade that is still in `Init`, and releases its coins. The nonces of such a
   trade are lost, so its setup cannot continue;
3. checks that the coins reserved in the wallet and the trades in the store agree: no coin stays
   reserved without a trade, and no trade is without its reservation;
4. watches the outputs of every trade that is not final, and evaluates the blocks since the
   deposit of each trade ([chain-observation.md](chain-observation.md));
5. performs the reactions for the events it finds, and continues the deadlines;
6. sends again every transaction with the status "sent".

After the trader signed the deposit, no MuSig2 nonce is needed any more: the penalty transaction
is signed with the full key, the claim with a single key, and the custom payout with wallet keys.
A restart in any state after `Init` therefore leads to the same final result as an uninterrupted
run.

## Trade IDs

A trade ID is accepted only once for a wallet, also after its trade is closed or abandoned, and
also after a restart. The set of used trade IDs is part of the backup.

## Secrets

- Secret key shares, the seed and passwords are never written to a log, at any log level.
- Secrets in memory are erased when they are no longer needed, as far as the libraries allow it.
- The store contains the peer's secret key share after a close. Together with the seed this is
  the key of a payout. The store must therefore be encrypted.

## Backup and restore

A backup of the trade store is needed in addition to the seed.

What the seed alone restores, for a trade whose ID is known:

- the trader's own key shares;
- with them, a cooperative close with the peer.

What the seed alone does not restore:

- the peer's public key shares and the trade parameters;
- the signatures of the warning and redirect transaction;
- the adaptor signature of the swap transaction;
- the peer's secret key share that was received at close, and so the key of a payout that is
  still in the deposit output.

Consequences:

- After a restore from the seed alone, an open trade can end only if the peer cooperates. The
  engine reports for such a trade that the trader cannot leave it alone.
- A payout of a closed trade that was not spent or drained is lost without the trade store.
  Drained coins belong to the seed wallet and are restored with the seed.
- After a restore from the seed and a backup of the trade store, every trade continues to the
  same result, provided the backup is not older than the last state change of the trade.

A backup that is older than the last state change of a trade is dangerous: the engine could repeat
a step with other data. The user documentation must say that a backup is taken after every state
change, or that the store is kept on storage that does not lose data.
