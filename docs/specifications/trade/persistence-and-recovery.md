# Persistence and recovery

This file defines what the engine stores about a trade, when it stores it, and what happens after
a restart or a restore.

## The trade store

The engine keeps all trades in one durable, encrypted store: the database of the wallet. One
record per trade is stored there. The wallet and its trades are therefore protected by the same
password, and one copy of the wallet database is a backup of both.

Rationale: a trade and the wallet depend on each other. The coins that a trade has reserved, the
keys of its payouts and the block at which its deposit was signed are data of the wallet. Two
stores could be restored from backups of different times, and would then contradict each other.

Stored for each trade:

- trade ID, role, trade parameters, the lock times and margins of the configuration at the time
  of the setup, state, and the terminated mark with its reason;
- the peer's public key shares, script key, addresses and half of the deposit;
- what is needed to have the deposit transaction and the trader's own fully signed warning and
  redirect transaction again;
- for the buyer, the adaptor signature of the swap transaction; for the seller, what is needed
  to have the swap transaction again and, once received, the buyer's partial signature for it;
- the peer's secret key share, once received, and with it the penalty transaction;
- the custom payout PSBTs of the trader and of the peer, and the final custom payout transaction;
- the trader's reserved coins;
- the height at which the trader signed the deposit;
- the height from which the deposit timeout is counted, while the deposit is unconfirmed
  ([setup-and-deposit.md](setup-and-deposit.md)).

The record of a trade is kept after the trade is closed or abandoned. The records are therefore
also the set of all trade IDs that were ever used.

The state that follows from the chain ([trade-states.md](trade-states.md)) is not stored. It is
derived from the chain again after a restart.

The store keeps no list of sent transactions and no status "sent". Which transactions the trader
must have on the network follows from the stored state of the trade and from the chain
([chain-observation.md](chain-observation.md), "Reactions to chain events"). A decision of the
trader that makes the engine send a transaction, for example the request to publish the warning
transaction, is a change of the stored state.

The deposit abort transaction is not stored either. Its inputs, its output and its fee follow
from the stored data of the trade, so the engine builds the same transaction again.

That the store keeps no list of sent transactions is a decision of the project owner of
2026-11-18 for now. It is looked at again later (`concept/CompletionPlan.md`, section 12,
row 10): a list becomes necessary with a transaction whose content cannot be derived again, for
example a replacement with a raised fee ([fees.md](fees.md)).

Stored once for the wallet:

- the drain transaction, while it is unconfirmed
  ([post-trade-protection.md](post-trade-protection.md)).

A trade whose deposit transaction the trader has not signed is stored with its ID, role, state
and reserved coins only. Its setup cannot continue after a restart (see "Restart").

Never stored:

- the trader's own secret key shares. They are derived from the seed when they are needed
  ([key-derivation.md](key-derivation.md));
- secret nonces.

## When the engine stores

**The new state is in the store before any signature, key share or signed transaction leaves the
engine.**

This holds for every response to the caller and for every transaction that the engine sends.

- The trade is stored from its first request on, so that its ID counts as used.
- The trade is stored after every request, also after a request that was refused: a refusal can
  have terminated the trade.
- If the trade cannot be stored, the request fails, and nothing that the request produced is
  handed out.
- A change that follows from the chain, for example a key share that the engine read from the
  swap transaction, or a forced close that the engine started on its own, is stored before the
  engine sends a transaction for it. If it cannot be stored, the engine sends nothing for this
  trade and tries again at its next look at the chain.

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

The trades are loaded when the wallet is opened, before the first request for a trade is
processed. The engine then:

1. loads all trades from the store;
2. abandons every trade that is still in `Init`, and releases its coins. The nonces of such a
   trade are lost, so its setup cannot continue. The trade stays in the store as abandoned;
3. makes the coins reserved in the wallet and the trades in the store agree: a reserved coin that
   no trade reserves is released, and a coin that a trade reserves is reserved again;
4. watches the outputs of every trade that is not final, and evaluates the blocks since the
   deposit of each trade was signed ([chain-observation.md](chain-observation.md));
5. performs the reactions for the events it finds, and continues the deadlines;
6. sends every transaction that the trader must have on the network and that is not confirmed.

Until step 4 is done for a trade, the engine does not know what happened to the trade on the
chain. In this time a request that needs a condition of the chain is refused as "condition of the
chain does not hold yet", and the caller can repeat it. The engine sends no transaction of the
trade in this time.

Rationale for step 3: `musigd` can stop between the reservation of a coin and the storing of its
trade. Only trades reserve coins, so a reserved coin without a trade belongs to a trade that was
never stored, and nothing of that trade left the engine.

### A record that cannot be loaded

If one record of the store cannot be loaded, for example because it is damaged, because it has a
format that the program does not know, or because it does not belong to the seed of the wallet:

- no trade is loaded, and the engine serves no request for a trade, also not for a new trade.
  The error names the trade and the reason;
- the reserved coins of the wallet stay reserved, and none of them can be released by hand;
- the other functions of the wallet keep working.

Rationale: an engine that knows a part of its trades would accept the ID of a trade that it does
not know for a new trade, would not watch the trade that it lost, and would release the coins of
that trade. The funds of the wallet that are in no trade must stay reachable.

How the user leaves this condition is open point O17 in [README.md](README.md).

After the trader signed the deposit, no MuSig2 nonce is needed any more: the penalty transaction
is signed with the full key, the claim with a single key, and the custom payout with wallet keys.
A restart in any state after `Init` therefore leads to the same final result as an uninterrupted
run.

## Trade IDs

A trade ID is accepted only once for a wallet, also after its trade is closed or abandoned, and
also after a restart. The set of used trade IDs is part of the backup.

## Secrets

- Secret key shares, the seed and passwords are never written to a log, at any log level.
- Secrets in memory are erased when they are no longer needed. The rules are in the section
  "Secrets in memory".
- The store contains the peer's secret key share after a close. Together with the seed this is
  the key of a payout. The store must therefore be encrypted.

## Secrets in memory

The secrets of a trade are: the trader's own secret key shares, the secret key share of the peer,
the secret key of a payout, and the secret nonces of the signatures.

- A secret that the engine or the wallet keeps beyond one computation is kept in a type that
  overwrites its memory with zeros when the value is dropped.
- Such a value is not copied without a visible copy in the code, and its debug output does not
  show the secret.
- A secret nonce is removed from its place when it signs, and is wiped after this one signature.
- The serialised form of a trade record, which contains the secret key share of the peer after a
  close, is wiped when it is dropped. Its buffer has its final size from the start, so that no
  earlier, smaller buffer with a part of the record stays behind.
- The seed words, the seed and the passwords are wiped by the wallet.

Limits. These copies of a secret are not wiped:

- the form that a cryptographic library needs for a computation. It is a plain value that the
  library copies. The engine creates it for one computation and does not keep it;
- the signing keys of the seed, which the wallet library keeps from the first signature to the
  end of the process;
- the messages of the service interface that carry a key share (the release and the close), in
  the buffers of the gRPC library. These messages cross the loopback interface without
  encryption ([service-interface.md](service-interface.md), security assumptions);
- the page cache of the database.

Rationale: a secret that stays in memory after its use can be read from a memory dump, from the
swap space, or by another defect of the process. Wiping does not protect against an attacker who
reads the memory of the running process while a trade is open: the secrets are needed then.

## Backup and restore

The design of the restore, from the seed alone and from a backup, is postponed
(`concept/CompletionPlan.md`, section 12, row 9; point W1 of the wallet specification). This
section states what is known today. Nothing of it is implemented or tested.

A backup of the trade store, which is a backup of the wallet database, is needed in addition to
the seed.

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
