# Plan to complete the Bisq MuSig trade protocol

Status: plan only, no code was changed. Revision 2, written 2026-11-17 against `main` at commit
`7a322e9`. Revision 2 includes the decisions D1 to D16 (section 7). Revision 1 listed them as open.

Revision 3 (same day) includes the answers to the open questions of revision 2. The questions and
answers are kept in section 12 for a later review.

WP1 was carried out on 2026-11-18 (branches `wp1-first-steps`, `wp1-move-trade-model`,
`wp1-finish`). Its result is described in section 8 under WP1. The file and line references of
the plan point to the code after WP1. The other text of the plan still describes the state of
revision 3; sections 2 and 4.3 and the findings name what WP1 changed.

The decisions D17 and D18 and the tests G13 to G15 were added on 2026-11-18.

The specification of WP0 was written on 2026-11-18 to `docs/specifications/trade/`. Its README
lists the open points that wait for a confirmation by the project owner. On the same day O2 was
decided (see D17), O6 was accepted (see N6), and O1 was postponed (section 12, row 8).

The identifiers of revision 1 are unchanged: findings F1 to F26, decisions D1 to D16, work packages
WP0 to WP8, test groups A to L. Revision 2 added findings F27 and F28, the points N1 to N9
(section 7.2) and test group M.

## 1. Purpose and method

This document answers three questions:

1. What does the project do today, and which concept documents still describe it correctly?
2. What is missing before a real trade can run safely, first on regtest and later on mainnet?
3. Which Rust tests are needed to cover every way a trade can complete and every way it can fail?

How the plan was produced:

- All Markdown files and diagram labels in `concept/` were read.
- The `protocol` crate, `rpc/src/protocol.rs`, `rpc/src/server.rs` and the trade `.proto` files were
  read completely.
- The `wallet`, `chain`, `mem`, `testenv` crates, the rest of `rpc`, the Java test clients and the
  CI workflow were surveyed. The most important survey results were checked again in the code.
  Results that were not checked again are marked "survey, not re-verified".
- The bisq2 checkout (`bac6dd1d1c`) was read for the Java side of the interface, mainly
  `docs/specs/trade/mu_sig/mediation/` and `trade/src/main/java/bisq/trade/mu_sig/`.
- The list of open upstream issues was read (29 issues, 2 pull requests on 2026-11-17).
- Verification run: `cargo test -p protocol --lib` (25 tests pass). No integration test was run.
- For revision 2, the code that the decisions depend on was read again: the compact block filter
  client (`chain/src/lib.rs`, `wallet/src/chain_data_source.rs`, the libraries `bdk_kyoto` 0.17.0
  and `bip157` 0.6.3), `drain_imported_balance` in `wallet/src/bmp_wallet.rs`, the claim script in
  `protocol/src/script_paths.rs`, the key share generation in `protocol/src/multisig.rs`, and
  `rpc/src/bin/musigd.rs`.

Effort numbers in this document are rough estimates for one developer who knows the code base.
They are not commitments.

## 2. Summary

The cryptographic core is in good shape. The transaction builders, the MuSig2 and adaptor
signature code, the deposit PSBT merging and the trade state model exist and have unit tests.
The persistent wallet (`BMPWallet`) and its gRPC service are real and tested.

The parts around the core are not connected yet. The main gaps are:

1. **Two protocol implementations exist.** (Resolved by WP1: `BMPProtocol` is removed, and the
   engine uses the wallet of the daemon.) `BMPProtocol` in the `protocol` crate is the older one.
   It works with real wallets in tests, but it has regtest values hard-coded and no state machine.
   `TradeModel` in `protocol/src/trade_model.rs` is the newer one that bisq2 uses. It has the state machine
   and the newer transaction design, but it runs on mock wallets.
2. **The trade service never touches the chain.** It does not broadcast any transaction, the
   confirmation streams are simulated, and the block height is the constant `900_000`.
3. **The trade service does not use the real wallet.** No coins are reserved, and the payout key is
   not imported into the wallet when a trade closes.
4. **Trade state is kept in memory only.** A restart of `musigd` during a trade loses the key
   shares and the pre-signed transactions. After the deposit is confirmed this means the funds can
   only be recovered if the peer cooperates.
5. **The dispute path is not reachable.** Warning, redirect, claim and penalty transactions are
   built and signed, but there is no RPC to publish them, no code that reacts to them, and
   `close_trade` ends in `todo!()` for these cases.
6. **Fee handling for pre-signed transactions is missing.** Anchor outputs exist, but the wallet
   cannot spend them to raise the fee (child-pays-for-parent, CPFP). The countermeasures from
   `pinningattack.md` are not implemented.
7. **Tests cover only a small part of the trade outcomes.** No test runs a complete trade with
   real wallets, real broadcasts and checked balances. No test covers a restart, a chain
   reorganisation, a dishonest peer, or the warning path through the state machine.
8. **Operational gaps.** Secret key shares can be written to the log, the wallet sync loop stops
   for good after one error, and CI does not run when only the `rpc` crate changes.

The decisions of section 7 fix the target design:

- `TradeModel` becomes the only engine (D1). It acts on its own for all time-critical reactions
  (D7).
- The chain is observed and transactions are broadcast only through compact block filters (CBF)
  and the Bitcoin peer-to-peer network. `musigd` no longer uses the RPC interface of a Bitcoin
  Core node (D2).
- The claim transaction is no longer pre-signed. The trader spends the warning escrow alone
  through the script path (D3). The redirect transaction gets a lock time of 1 block (D4).
- Lock times and amounts become explicit trade parameters (D5). The deposit needs 3 confirmations
  (D6).
- Key shares are derived from the wallet seed (D10).
- `musigd` watches closed trades while it runs, which is only while bisq2 runs. When the user ends
  the program, Java calls a new `close` method of the wallet service. This method moves all
  imported coins to the seed wallet (D8).
- The engine takes the fee rate for its own transactions from one method. For now this method
  returns a fixed value of 10 sat/vB (N1).
- The gRPC port has no authentication (D11). `walletrpc.Wallet` and the `mem` crate are removed
  now (D16).

Order of work: write the specification (WP0), merge the two implementations into one engine,
connect wallet and chain, add persistence, complete the dispute path, then harden and test on
signet before mainnet. Section 10 gives milestones.

## 3. How to read the concept folder

`concept/` keeps the history of the design. The authoritative specification will be written to
`docs/specifications/trade/` (D15, WP0). Until that exists, this table says which concept files can
be trusted.

| Document | Status | Comment |
|---|---|---|
| `STx8.drawio` (+ `.svg`) | Current, will change | Best description of the transaction graph. Its own note says it was synced with the code on 2025-09-28. It matches `TradeModel`, not `BMPProtocol`. D3 and D4 change the claim and redirect parts. |
| `pinningattack.md` | Current | Matches the code. None of its countermeasures is implemented. D4 adopts its redirect lock time. |
| `MuSig2adaptor-rust.md`, `MuSig2adaptor.md` | Current (mathematics) | Describes key aggregation and adaptor signatures. The code follows it (`protocol/src/multisig.rs`). |
| `bisq-musig-modules.drawio`, `bisq2-overview.drawio` | Current (target picture) | Shows the intended modules. "Protocol persistence" and "chain access for the protocol" in the picture do not exist yet. |
| `SingleTxOverview.md` | Partly outdated | The idea is still valid. Outdated details: the deposit transaction is described with 2 inputs and 4 outputs; fees and the custom payout are not covered. The redirect delay t1 that it describes returns with D4, with a value of 1 block. |
| `update.md` | Partly outdated | "ClaimTx is pre-signed" is true for the code today and is reversed by D3. "No CSV scripts" is no longer true: the warning escrow has a CSV claim script leaf. "Only the seller has a SwapTx" is implemented. |
| `ProtocolUpdate2.md` | Partly implemented | Penalty transaction created by the protocol: done. Claim by script: the script leaf exists, nothing spends through it; D3 makes it the only claim path. Redirect delay removed: done in the code, reversed by D4. The chain observation rules in this file are not implemented. |
| `blindedsig/blindedsig-swaptx.md` | Implemented in `TradeModel` only | The buyer signs only the sighash of the swap transaction. The file ends without a conclusion about the attack it describes. |
| `ProtocolSubStates.md`, `StateMachine.drawio` | Outdated | Replaced by `protocol/src/state.rs`. The link to a test in `protocol/src/lib.rs` is dead. |
| `API.md` | Outdated as a description, valid as a requirement list | The automatic reactions listed there (timeouts, automatic broadcasts) are not implemented anywhere. D7 and D13 adopt them. |
| `partialTransaction.md` | Outdated | Replaced by the half-PSBT design in `protocol/src/psbt.rs`. |
| `watchtower/OverviewWatchTowers.md` | Not used | D8: `musigd` itself watches, and imported coins are moved when the wallet closes. External watchtowers are not planned. |
| `tradefees/`, `silentpayment/` | Concept only | No code, except the generic `trade_fee_receivers` list in the deposit builder. D9 uses one fee output in the deposit transaction. |
| `chainaccess/mempool-with-ZMQ.md` | Obsolete | D2 and D16: no Bitcoin Core interface, the `mem` crate is removed. The engine sees confirmed transactions only. |

Outside this folder, `rpc/musig-trade-protocol-messages.txt` and the bisq2 specifications in
`docs/specs/trade/mu_sig/mediation/` describe the current message flow better than `concept/`.

## 4. The protocol: implemented today and decided target

Names: the seller sells bitcoin and receives the fiat payment. The buyer pays fiat. `T` is the trade
amount, `Db` and `Ds` are the security deposits of buyer and seller. `P'` and `Q'` are MuSig2
aggregated keys. `P'` locks the buyer payout, `Q'` locks the seller payout. Each trader holds one
key share of each.

### 4.1 Transactions

| Transaction | Spends | Pays to | Signed today | Lock time today (mainnet / regtest) | Target after the decisions |
|---|---|---|---|---|---|
| Deposit | Wallet coins of both traders | Buyer payout `T + Db` locked by `P'`; seller payout `Ds` locked by `Q'`; change; optional trade fee output | Each wallet signs its own inputs | none | Unchanged. One fee output (D9). Counts as confirmed at 3 confirmations (D6). |
| Swap (seller only) | Seller payout | Seller wallet | MuSig2 adaptor signature; using it reveals the seller's share of `P'` to the buyer | none | Unchanged |
| Warning (one per trader) | Both payouts | Escrow output; anchor output of 330 sat for the publisher | MuSig2, pre-signed | 1440 / 5 blocks after the deposit confirms | Lock time becomes a trade parameter (D5) |
| Redirect (one per trader) | Escrow of the *peer's* warning | Burning men of the DAO; anchor output | MuSig2, pre-signed | none | Lock time 1 block (D4) |
| Claim (one per trader) | Escrow of the trader's *own* warning | Own wallet | MuSig2, pre-signed, key path | 720 / 5 blocks after the warning confirms | Not pre-signed. Built and signed by the claimer alone through the script path, with the fee rate of that moment (D3). Lock time is a trade parameter. |
| Penalty (one per trader) | Escrow of the peer's warning | Own wallet | Alone, with the full key learned in the key exchange | none | Unchanged |
| Custom payout | Both payouts, by the 2-of-2 script path | Both wallets, amounts from mediation | Both wallets with script keys | none | Unchanged |

Both payout outputs also contain a 2-of-2 script leaf with one wallet key of each trader. Only the
custom payout uses it.

How the warning escrow is built today (`set_warning_escrow_keys` in
`protocol/src/trade_model.rs:1285-1342`, `protocol/src/script_paths.rs:18-49`):

- The escrow of the buyer's warning has `Q'` as internal key. Its only script leaf is
  `<buyer's key share of P'> CHECKSIGVERIFY <claim lock time> CSV`.
- The escrow of the seller's warning has `P'` as internal key. Its script leaf uses the seller's
  key share of `Q'`.
- In a cooperative close the seller gives away his share of `P'` and the buyer gives away his
  share of `Q'`. The two key shares used in the claim scripts are never given to the peer. For
  this reason the claim by script (D3) needs no new key and no change of the escrow output.

### 4.2 Trade outcomes

| Outcome | What happens | Final distribution |
|---|---|---|
| Cooperative close | Seller sends his share of `P'`, buyer answers with his share of `Q'`. No transaction is needed. | Buyer owns `T + Db`, seller owns `Ds`, both still in the deposit outputs |
| Forced close | Seller publishes the swap transaction because the buyer does not answer. The buyer reads the key share from the signature. | Same as above; seller pays the swap fee |
| Custom payout | Both sign the mediated payout. | As mediated |
| Redirect | One trader publishes the warning, the other answers with the redirect. | All funds go to the burning men. The DAO arbitrator reimburses. |
| Claim | One trader publishes the warning, the other does not react within the claim lock time. | The publisher gets everything |
| Penalty | A trader publishes the warning *after* a cooperative or forced close. | The other trader takes everything |
| Setup abort | The setup stops before the deposit confirms. | Nobody loses funds; reserved coins must be released |

### 4.3 The two implementations

D1: `TradeModel` becomes the single engine. `BMPProtocol` is removed after its integration tests
are ported.

Done in WP1. The table shows the state before WP1; the files and lines of its `BMPProtocol` column
no longer exist. After WP1 the engine takes the wallet as a parameter of its operations, the lock
times are trade parameters, and the mock wallets are behind the feature `test-utils`. The engine
still does not broadcast (F3).

| Topic | `BMPProtocol` (`protocol/src/protocol_musig_adaptor.rs`) | `TradeModel` (`protocol/src/trade_model.rs`) |
|---|---|---|
| Interface | Five rounds, `bmp_protocol.proto`, served only by a test daemon | Ten RPCs in `rpc.proto` (`musigrpc.Musig`), used by bisq2 |
| Wallet | Real (`BMPWallet` or `MemWallet`) | Mock wallets from `protocol::mocks` |
| Broadcast | Yes, through `ChainApi` | No (`rpc/src/server.rs:280`, `:356`, `:417`) |
| Network | `Network::Regtest` hard-coded in 9 places | Taken from the wallet |
| Lock times | Warning 0, redirect 1, claim 2 blocks (`:385`, `:447`, `:569`) | `NetworkParams` in `protocol/src/transaction.rs` |
| Fee rates | 10 and 20 sat/vB hard-coded | Supplied by Java per trade |
| Warning escrow | Key spend only | Key spend plus CSV claim script leaf |
| Swap signing by buyer | Buyer builds the swap transaction | Buyer signs the sighash only (blind) |
| State machine | Round counter only | `TradeState` with role-specific transition rules |
| Custom payout, penalty | Penalty only | Both |
| Peer input errors | `assert!` panics (`:207`, `:282`) | Errors. The `todo!()` in `close_trade` is replaced by an error (`:815`). |
| Debug output | `dbg!` calls and a log line that prints a private key (`:208`) | None found |

## 5. Current state per component

| Component | State | Missing |
|---|---|---|
| Transaction builders (`protocol/src/transaction.rs`) | Complete, unit tested with fixed transactions | Explicit trade parameters (D5); redirect lock time 1 (D4); claim by script path (D3) |
| Deposit PSBT merge (`protocol/src/psbt.rs`) | Complete, unit tested | Nothing. The peer's inputs are not checked on chain (D12). |
| MuSig2 and adaptor signing (`protocol/src/multisig.rs`) | Complete | Key shares from the wallet seed (D10); zeroising of secrets |
| Trade state model (`protocol/src/state.rs`) | Complete as a model, unit tested | Not persisted; warning states are never entered |
| Trade engine | Two partial versions (4.3) | One engine with wallet, chain, persistence, timers |
| Wallet `BMPWallet` | Real: encrypted, seed, CBF sync, imported keys, coin locks, `drain_imported_balance` | Section 6.2; fee bumping; recording of external transactions; key share derivation (D10) |
| Wallet gRPC `wallet.Wallet` (`bmp_wallet.proto`) | Real, tested in Rust and Java. bisq2 contains a client for it (`wallet/src/main/java/bisq/wallet/WalletGrpcClient.java`); whether bisq2 uses the client in normal operation was not checked. | Notifications; a `Close` RPC that drains the imported coins (D8) |
| Old wallet gRPC `walletrpc.Wallet` (`wallet.proto`) | Test wallet with a fixed regtest descriptor | Removed in WP1 (D16) |
| Chain access (`chain`) | Broadcast trait; CBF scanner that builds a new client for every wallet sync | Long-running CBF client; watching scripts and outpoints; block height; reorganisation events; broadcast over the peer-to-peer network (D2) |
| `mem` crate | ZMQ transaction stream; no crate uses it | Removed in WP1 (D16) |
| `musigd` | Starts the services on `127.0.0.1`. Broadcasts wallet transactions through Bitcoin Core RPC (F27). | Drain when Java calls `close` (D8), network checks, configuration, removal of the Bitcoin Core options (D2) |
| `testenv` | bitcoind 29.0 and electrs on regtest, mining, funding; block filters are enabled | Helpers for reorganisations, mempool checks, lock time maturity, node restart |
| Tests | See section 9.1 | See section 9 |
| CI | Builds and tests when `wallet/**` or `protocol/**` change | `rpc` changes, formatting, dependency audit, Java tests |
| bisq2 Java side | Calls all ten `Musig` RPCs; mediation and arbitration messaging exist | Confirmation handling is disabled, deposits are fixed at 30,000 sat, no warning path, no recovery after RPC errors |

## 6. Findings that affect the plan

"Confirmed" means the code was read and the behaviour follows from it. No finding was reproduced
with a running test.

### 6.1 Trade protocol and `rpc`

| # | Finding | Evidence | Impact |
|---|---|---|---|
| F1 | Trade state exists only in memory. | `MemoryTradeStore` in `protocol/src/trade_model.rs:48`, FIXME at `:478` | After the deposit confirms, a restart removes every unilateral exit for that trader. Confirmed. |
| F2 | `InitTrade` with an existing trade id replaces the stored trade. | Was `rpc/src/protocol.rs:34`. Fixed in WP1: `add_trade_model` rejects a stored trade id (`protocol/src/trade_model.rs:55`). | A repeated or hostile local call destroys the key shares of a running trade. With D10 a repeated trade id would also produce the same key shares again (N6). Confirmed. |
| F3 | No transaction is broadcast and no confirmation is observed. | `rpc/src/server.rs:146`, `:280`, `:299`, `:356`, `:417`, `:434` | The service cannot run a real trade. Confirmed. |
| F4 | The dispute outcomes cannot be reached. | The `todo!()` was at `rpc/src/protocol.rs:795`. Replaced by an error in WP1 (`protocol/src/trade_model.rs:1783`). No RPC publishes a warning. | Half of the state machine is unused. A `todo!()` panic also poisons the trade mutex, so later calls for this trade panic too (`rpc/src/server.rs:121`). Confirmed. |
| F5 | The payout key is checked but not imported into the wallet at close. | TODO at `protocol/src/trade_model.rs:1765` | After a cooperative close the user cannot spend the payout. Confirmed. |
| F6 | The seller can release near or after the buyer's warning lock time. | Comment at `protocol/src/trade_model.rs:758-762` | The buyer's warning transaction can replace the swap transaction. The seller then has no penalty transaction and can only redirect. Confirmed as a design gap. |
| F7 | Private key shares are written to the log at debug and trace level. | `rpc/build.rs:37`, `:53`, `:56` serialise them as base64; `rpc/src/server.rs:591-623` logs requests and responses | Secrets on disk when verbose logging is on. `BMPProtocol` also logged a private key; it was removed in WP1. Confirmed. |
| F8 | No authentication or TLS on the gRPC port. | `rpc/src/bin/musigd.rs:80-132` | Any local process can call `SignSwapTx` or `CloseTrade` for a known trade id, or use the wallet service. Confirmed. D11 accepts this risk; see section 7.2, N7. |
| F9 | The engine does not check chain conditions before it releases secrets. | `start_payment`, `receive_swap_tx_signature`, `release_payout` in `protocol/src/trade_model.rs:709-777` | Rust trusts Java that the deposit is confirmed. The component that holds the secrets should enforce this rule. Confirmed. D6 defines the rule: 3 confirmations. |
| F10 | The claim script leaf of the warning escrow is never used or tested on chain. | `protocol/src/script_paths.rs:18`, `protocol/src/trade_model.rs:1285` | With D3 this leaf becomes the only claim path, so it must be implemented and tested on chain (WP4, tests E3, E4, E11). Confirmed. |
| F11 | Mock wallets are part of the production build. | Was `protocol/src/lib.rs:1` (`pub mod mocks`). Fixed in WP1: the module is behind the feature `test-utils`, and `musigd` uses the wallet of the wallet service. | Fixed keys and signatures are reachable from production code. Confirmed. |

### 6.2 Wallet and chain

| # | Finding | Evidence | Impact |
|---|---|---|---|
| F12 | Addresses and internal keys are issued again after 50 unused ones. | `wallet/src/bmp_wallet.rs:194-227`, used by `new_address` and `new_internal_key` at `:792-801` | A trade uses 4 to 5 wallet keys that normally never receive funds. After about 10 trades, keys and addresses of earlier trades are reused in new trades. The rotation position is not persisted. Confirmed. |
| F13 | Coins selected for a deposit are not reserved. | `create_psbt` at `wallet/src/bmp_wallet.rs:803`; the lock API exists but the protocol does not call it | Two trades, or a trade and a payment, can select the same coin. Confirmed. Upstream issues #59 and #185. |
| F14 | The wallet cannot record a transaction that it did not create itself. | No such method; only `send_to_address` records its transaction | The wallet learns about the deposit only when a block is scanned. Confirmed (survey). |
| F15 | Every transaction spends all unlocked imported coins. | `wallet/src/coin_selection.rs:22-51` | This moves trade payouts early, which protects against a late warning transaction. It also links trades on chain and ignores coins that the caller selected by hand. D8 keeps this rule. Confirmed. |
| F16 | `BMPWallet::sign` panics when a PSBT input has no `witness_utxo`. | `wallet/src/bmp_wallet.rs:1008` | A PSBT from the peer can stop the process. `psbt.rs` checks this for the deposit, so the risk is in other callers. Confirmed. |
| F17 | Database files of imported keys are not encrypted. | `wallet/src/bmp_wallet.rs:677` opens the file without `PRAGMA key`; compare `:727-729` | Descriptors with the peer's keys and the transaction history are stored in plain SQLite. The secret keys are in the encrypted main database. Confirmed. |
| F18 | The sync loop ends after the first error. | `rpc/src/bmp_wallet_service.rs:289`, `:307` | No more wallet updates until restart. Upstream issue #190. Confirmed. |
| F19 | A new imported key makes the next sync start at the genesis block. | `load_imported_wallet` creates the sub-wallet with `Wallet::create_single` and sets no checkpoint (`wallet/src/bmp_wallet.rs:688-690`). Every sync uses `ScanType::Sync` (`wallet/src/chain_data_source.rs:37`). `bdk_kyoto` 0.17.0 starts the filter scan at the lowest checkpoint of all wallets (`build_with_wallets` in its `builder.rs`). The constant `RECOVERY_HEIGHT` is defined but not used. | On mainnet every closed trade would start a filter download from height 0. With D2 there is no other chain backend, so this must be fixed in WP2: the sub-wallet must start at the height of the deposit. Confirmed by reading the code; not reproduced with a test. Upstream issue #202 is related. |
| F20 | No fee bumping: no CPFP for anchors, no replace-by-fee. | No code; upstream issue #187 | A pre-signed transaction with a low fee cannot be accelerated. |
| F21 | No code watches a transaction id or an outpoint. | No code; upstream issue #61 | Required for every reaction of the protocol. |
| F22 | `CBFScanner::from_hostnames` panics on malformed input. | `chain/src/lib.rs:113`, `:116` | Start-up crash on a configuration error. Upstream issue #193. Confirmed. |
| F27 | `musigd` broadcasts wallet transactions through Bitcoin Core RPC. | `BitcoinCoreChainApi` in `rpc/src/bmp_wallet_service.rs:512-526`, used in `rpc/src/bin/musigd.rs:86` | D2 forbids this path. Broadcast must move to the peer-to-peer network (`submit_package` of the `bip157` client). After that and D16, `musigd` needs no Bitcoin Core RPC options. Confirmed. |
| F28 | Every wallet sync builds a new CBF client and stops it after the first update. | `sync_cbf` in `chain/src/lib.rs:160-184` | Nothing observes the chain between two syncs, and each sync opens new peer connections. The engine needs one long-running client that the wallet sync shares. Confirmed. |

### 6.3 Tests and CI

| # | Finding | Evidence |
|---|---|---|
| F23 | The `musigrpc.Musig` service has no Rust test. Its only driver is a Java `main` program without assertions. | `rpc/tests/`; `rpc/src/main/java/bisq/TradeProtocolClient.java` |
| F24 | The protocol integration tests do not check results. They publish transactions but do not check balances, states or confirmations. `test_swap` passes the swap transaction directly instead of reading it from the chain. | Was `protocol/tests/protocol_integration_tests.rs:150-281`. Fixed in WP1 for the ported scenarios: `protocol/tests/trade_model_integration_tests.rs` checks confirmations, amounts and states, and the buyer reads the swap transaction from the block. Balances of the wallets are not checked yet. |
| F25 | CI starts only for changes in `wallet/**` or `protocol/**`. A change in `rpc`, `chain` or `testenv` alone is not built or tested. | `.github/workflows/cont_integration.yml:22-26` |
| F26 | No formatting check, no dependency audit, no Java tests in CI. One Rust test is ignored as broken. | Workflow file. The ignored test was `rpc/tests/test_wallet_service.rs:12`; the file was removed in WP1 with the test wallet (D16). |

## 7. Decisions

### 7.1 Decisions taken

The decisions D1 to D16 were taken by the project owner on 2026-11-17, D17 and D18 on 2026-11-18.

| # | Question | Decision | What follows for the work |
|---|---|---|---|
| D1 | Which implementation becomes the single engine? | `TradeModel`. | Move it into the `protocol` crate. Remove `BMPProtocol` after its integration tests are ported (WP1). |
| D2 | How does the engine observe the chain? | Compact block filters over the peer-to-peer network only. No direct use of Bitcoin Core RPC. Reason: privacy. | One implementation of the chain interface. The engine sees confirmed transactions only. Broadcast goes to peers and returns no acceptance result. `musigd` loses its Bitcoin Core options (F27). The tests still start bitcoind, as the peer that the client connects to and for assertions. Points N1 to N4. |
| D3 | Claim by pre-signed transaction or by script? | By script spend. | The claim transaction leaves the set of pre-signed transactions: two signing sessions and three protobuf fields less. The claimer builds and signs it alone, with a current fee rate, and can replace it with a higher fee. The escrow output does not change (section 4.1). |
| D4 | Redirect lock time of 1 block? | Yes. | Constant of the protocol, not a trade parameter. A redirect can be sent when the warning has 1 confirmation. |
| D5 | Final lock times; values per payment method? | Explicit trade parameters that both peers agree on and that are part of the contract. `NetworkParams for Network` is removed. | Java passes the parameters at trade start: since WP1 with `GetNonceShares` (open point O13 of the specification). The engine checks limits and that both peers use the same values. Tests can use any value. The values per payment method are still open (N9). |
| D6 | Confirmations before the payment may start? | A constant with the value 3. | `MIN_CONFIRMATIONS = 3` in the `protocol` crate. The engine refuses to sign the swap transaction and to release a key share below this depth (F9). The handling of reorganisations is specified in WP0 (N8). |
| D7 | Who starts time-critical reactions? | The engine, automatically, while `musigd` runs. `musigd` runs only while bisq2 runs (section 12, answer 7). | Penalty, redirect after the peer's warning, claim, and the swap transaction before the warning lock time are started by the engine. Java receives events and decides only where the protocol allows a choice. |
| D8 | Protection after the trade is closed. | `musigd` keeps watching closed trades. When the wallet closes, all imported coins are moved to the seed wallet with `drain_imported_balance`. "The wallet closes" means: Java calls a new method `close` of `BmpWalletService` when the user ends the program (section 12, answer 1). Spending imported coins first stays mandatory. | Watching in WP4. A new `close` method and a `Close` RPC that build, sign, store and send the drain transaction (WP2, WP4, WP7). F15 stays as it is. Point N5. |
| D9 | Trade fee payment in the first version. | One fee output in the deposit transaction. | Supported by the builder. Silent payments and fee channels are not part of this plan. |
| D10 | Random or derived key shares? | Derived from the wallet seed. | A new wallet method derives the two key shares of a trade. The derivation rule is part of the specification. Nonces stay random and are never stored. Point N6. |
| D11 | Authentication for the gRPC port. | None. | No work. The port stays bound to the loopback address. The risk of F8 is accepted (N7). |
| D12 | Check the peer's deposit inputs on chain? | No. | No work. A deposit that cannot confirm is handled by the abort of D13. |
| D13 | Deposit signed but not confirmed. | After a timeout, spend the own deposit input back to the own wallet with a higher fee. | The engine cannot see the mempool (D2), so the timeout counts blocks without a confirmation. The engine follows the transaction that confirms. Needs replace-by-fee support in the wallet (WP2). |
| D14 | Minimum trade amount and deposit. | Derived from the dust limit and the fee rate, so that every pre-signed transaction has valid outputs. Smaller trades are rejected in `GetNonceShares`. | Validation in the engine (WP4). |
| D15 | Where do authoritative specifications live? | `docs/specifications/trade/`. `concept/` keeps the history. | WP0 writes the specification files. Concept files are not updated; section 3 says which ones are outdated. |
| D16 | Future of `walletrpc.Wallet` and the `mem` crate. | Remove both now, also if this breaks bisq2. | First step of WP1. bisq2 at commit `bac6dd1d1c` contains no reference to `walletrpc` (text search), so no break is expected there. |
| D17 | When does a failed check terminate a trade? | Everything by which the engine detects that the other side tampered with the software or does not run our client terminates the trade. The reason is logged in detail. The trade is unusable afterwards; there is no recovery. Other conditions, which can also happen because of network issues for example, can be repeated. (First form of the decision on the same day: "anything which fails the validation terminates the trade".) | The specification lists the checks of each kind (`docs/specifications/trade/trade-states.md`). The engine marks the trade as terminated, logs the reason, accepts no corrected data and does not change the state. Implemented so far for the check of the peer's nonce shares only (G4). The other checks of peer data do not terminate the trade yet (WP1). Which actions a terminated trade keeps after the deposit is signed is postponed (section 12, row 8). Tests G13, G15. |
| D18 | In which order does the engine process a request? | Validate before storing, and check the state before any change. | Every engine operation checks the state first, then validates all data of the request, and only then stores data or signs. A refused request leaves no data and no signature in the trade. The handlers in `rpc/src/server.rs` do not follow this order yet: several store request data before the validation, and `SignCustomPayoutTx` signs before the state check (WP1). Tests G14. |

What D8 and answer 7 of section 12 mean together: `musigd` does not run while bisq2 is closed, so
nothing is watched in that time. For closed trades the drain at `close` is then the only
protection. For open trades the user must start the program often enough: after the peer's
warning transaction confirms, the redirect or penalty must confirm before the claim lock time
ends. The specification (`post-trade-protection.md`) must state this as a requirement for the
user.

### 7.2 Points that the decisions open

These points do not question the decisions. They are consequences that the specification (WP0)
must settle. The last column gives the status after the answers of section 12.

| # | Point | Proposal | Status |
|---|---|---|---|
| N1 | **Fee rate source (D2, D7).** The engine builds transactions on its own: claim, CPFP children, drain, deposit abort. A CBF client has no fee estimator. The `bip157` client offers only the minimum relay fee of the connected peers (`broadcast_min_feerate`) and the average fee rate of a block (`average_fee_rate`). | Java sends a current fee rate to `musigd` at regular intervals through a new RPC. The engine uses it with a lower limit from `broadcast_min_feerate` and an upper sanity limit. If no recent value exists, the engine uses the fee rates of the last blocks. | Postponed (answer 3). The real source is supplied later. For now one method returns a fixed 10 sat/vB and carries a TODO. Every transaction that the engine builds on its own takes its fee rate from this method. Limit of the fixed value: when the network needs more than 10 sat/vB, these transactions can stay unconfirmed, also the time-critical ones. |
| N2 | **Broadcast has no result (D2).** `submit_package` returns when the data was sent to one peer. The engine does not learn whether a node accepted the transaction. | The engine checks everything it can before sending (lock time reached, inputs unspent as far as known). A sent transaction has the state "sent" until it is seen in a block. The engine sends it again at intervals until it confirms or a conflicting transaction confirms. Test assertions about the mempool use bitcoind RPC in `testenv` only. | Not asked. The proposal stands until WP0. |
| N3 | **CPFP over the peer-to-peer network (D2).** `bip157` 0.6.3 accepts a package of one parent and one child. Whether a parent below the minimum relay fee is accepted this way depends on the receiving nodes. I believe Bitcoin Core supports this since version 28.0, but I did not verify it. | Verify with a regtest test against bitcoind 29.0 early in WP3, before WP6 depends on it. If it does not work, the pre-signed fee rates need a larger safety margin. | Assumed to work (answer 4). Test J7 stays in the plan and shows whether the assumption holds for bitcoind 29.0. |
| N4 | **Peers (D2).** The client must connect to peers that serve block filters. The peers see the IP address that broadcasts the trade transactions. | Out of scope for this plan (issues #127, #129). The specification states the limit. The peer list stays a start option of `musigd`. | Not asked. The proposal stands until WP0. |
| N5 | **Limits of the drain at wallet close (D8).** (a) The drain runs only when Java calls `close`. If bisq2 crashes, or the process is killed or loses power, nothing is drained. (b) The drain transaction is only sent, not confirmed, when the process ends. (c) A drain links all open payouts in one transaction. (d) Imported coins that are locked for an open trade are not drained (existing behaviour of `drain_imported_balance`). | Store the signed drain transaction before sending it. At the next start, check whether it confirmed; if not, send it again or replace it with a higher fee. Document the remaining risk: after an unclean stop, a dishonest peer can win the payout if the user stays offline until the peer's warning lock time and the claim lock time have both ended. | Trigger clarified (answer 1): the `close` method. `musigd` does not run while bisq2 is closed (answer 7), so the limits (a) and (b) matter. The proposal stands. |
| N6 | **Derivation rule (D10).** The rule must give different key shares for every trade, must not collide with wallet keys, and must not be computable from a public key of the wallet. | A hardened BIP32 path that the BIP86 wallet does not use gives a trade root secret. Key share = tagged hash of (trade root secret, trade id, label "buyer payout" or "seller payout"), reduced to a valid scalar. A trade id is never accepted twice, also not after the trade is closed (F2). The specification contains test vectors. | Accepted (answer 5). Path `m/86'/coin_type'/0'/2'` and tag `BisqMuSig/key-share` accepted on 2026-11-18; specified with test vectors in `docs/specifications/trade/key-derivation.md`. |
| N7 | **Accepted risk (D11).** Every local process, also of other users on the same computer, can call all RPCs: close a trade, sign the swap transaction, send wallet funds while the wallet is unlocked. Reading the seed phrase needs the wallet password. | Record this as a security assumption in the specification: the computer and all its local users are trusted. Keep the fixed loopback bind address and test it (K4). | Not asked. The proposal stands until WP0. |
| N8 | **Reorganisations (D6).** The constant covers the deposit. The rule for other transactions is not decided. | Deposit: below 3 confirmations no secret leaves the engine; if a reorganisation lowers the depth below 3 before the payment started, the state goes back. Reactions to a peer transaction (redirect, penalty) start at 1 confirmation, because waiting costs time before the claim lock time ends. A trade is final, and watching ends, when its closing transaction has 3 confirmations. | Accepted (answer 6). |
| N9 | **Parameter values (D5).** Which payment methods need which lock times is not decided. The warning lock time (10 days on mainnet today) is also the latest time by which a seller must have received the fiat payment or started mediation. The timeout of D13 and the safety margin of F6 also need values. | Decide with bisq2. The engine only enforces limits, for example: warning lock time larger than `MIN_CONFIRMATIONS` plus the F6 margin. | Answered (answer 2): the values are configurable. The plan fixes no values. Lock times are trade parameters (D5). The deposit timeout and the release safety margin are configuration values of the engine with defaults. |

What a restore from the seed gives after D10: the trader's own key shares of every trade whose
trade id is known. It does not give the peer's public key shares, the trade parameters, the
aggregated signature of the warning transaction, or the peer's secret key share received at
close. A cooperative close is possible from the seed and the trade id. Every unilateral exit
(warning, redirect, penalty, and spending a payout after close) still needs the trade store.
The trade store and its backup (WP5) therefore stay necessary. The drain of D8 reduces this
dependency, because drained coins belong to the seed wallet.

## 8. Work packages

The packages are ordered by dependency. Tests belong to each package; section 9 lists them.

### WP0 Specification (estimate: 1 to 2 weeks)

- Write the authoritative specification in `docs/specifications/trade/` (D15). Proposed files:

  | File | Content |
  |---|---|
  | `README.md` | Index and relations between the files |
  | `transactions.md` | Transaction graph, outputs, scripts, weights, anchors |
  | `parameters.md` | Trade parameters (D5), constants (D4, D6), minimum amounts (D14), limits |
  | `key-derivation.md` | Derivation of key shares, test vectors (D10, N6) |
  | `setup-and-deposit.md` | Message flow, validation of peer data, deposit, abort (D13) |
  | `closing.md` | Cooperative close, swap transaction, custom payout |
  | `dispute.md` | Warning, redirect, claim by script (D3), penalty |
  | `chain-observation.md` | What is watched, confirmations, reorganisations, required reaction to every event (D2, D6, D7, N2, N8) |
  | `post-trade-protection.md` | Watching closed trades, drain at wallet close, imported coins first (D8, N5) |
  | `fees.md` | Fee rate sources, CPFP, replacement (N1, N3) |
  | `persistence-and-recovery.md` | What is stored and when, restart, restore from seed and from backup |
  | `service-interface.md` | Meaning of each RPC, repeated calls, errors, security assumptions (D11, N7) |

- Write the answered points of section 7.2 into the specification, and settle the points that
  were not asked (N2, N4, N7).
- Write down the security argument for blind signing of the swap transaction. The concept file
  leaves it open.

Done when: every state transition and every chain event has a specified reaction for both roles.

### WP1 One protocol engine (estimate: 2 to 3 weeks; upstream issue #62)

- First step, independent of the rest (D16): remove `walletrpc.Wallet` and `mem`.
  - `rpc/src/main/proto/wallet.proto`, the generated `rpc/src/pb/walletrpc.rs`, `WalletImpl` in
    `rpc/src/server.rs`, its test wallet in `rpc/src/wallet.rs`, the wallet commands of
    `musig-cli`, `rpc/src/main/java/bisq/WalletClient.java`, and the entries in `rpc/build.rs`,
    `rpc/src/pb/convert.rs`, `rpc/src/lib.rs` and `rpc/README.md`.
  - The `mem` directory and its workspace entry; the ZMQ helpers in `testenv`, which only
    `mem/tests/zmq.rs` uses.
- Move `TradeModel` from `rpc/src/protocol.rs` into the `protocol` crate. The `rpc` crate keeps only
  the conversion between protobuf messages and engine calls. This step is a pure refactoring.
- Give the engine its dependencies as parameters: wallet (`ProtocolWalletApi`), chain interface,
  trade store, trade parameters.
- Replace `NetworkParams for Network` by a trade parameter structure (D5). Redirect lock time 1
  (D4) and `MIN_CONFIRMATIONS = 3` (D6) are constants. The deposit timeout (D13) and the release
  safety margin (F6) are configuration values of the engine with defaults (N9).
- Port the scenarios of `protocol/tests/protocol_integration_tests.rs` to the engine.
- Remove `BMPProtocol`, `bmp_protocol.proto`, `rpc/src/pb/bmp_converter.rs`,
  `rpc/tests/test_bmp_service.rs`, `BmpClient.java` and `BmpServiceIntegrationTest.java`.
- Move `protocol::mocks` behind a test feature (F11).
- Replace panics on input from the peer or from the RPC caller by errors (F4). Reject a duplicate
  trade id (F2).
- Apply D17 and D18 in every engine operation: check the state, then validate all data, then
  store or sign. Terminate the trade, with a detailed log entry, for every check of peer data
  that the specification lists; refuse without a change in all other cases.

Done when: one engine exists, the ported tests pass with `BMPWallet`, no production path uses a
mock wallet, and the workspace contains neither `walletrpc` nor `mem`.

Status on 2026-11-18: done, with these differences to the list above.

- The engine has no chain interface yet. No operation of the engine uses the chain before WP3,
  so the parameter is added there, together with the first use (broadcast of the deposit).
  The ported tests send the transactions to the node themselves.
- The trade parameters arrive with `GetNonceShares`, not with `InitTrade`. `rpc.proto` got the
  fields `warningLockTime` and `claimLockTime`. bisq2 must send them (WP7).
- `musigd` uses the wallet of the wallet service for trades. A trade needs `--wallet-dir` and an
  open, funded wallet. The development flow of bisq2 with mock data no longer works.
- The values of `EngineConfig` (deposit timeout 144 blocks, release safety margin 6 blocks) are
  placeholders until N9 is decided, and `musigd` has no option for them yet.
- The confirmation depth (D6) and the release safety margin (F6) are constants and configuration
  values now, but nothing enforces them before the engine sees the chain (WP3, WP4).
- The signatures of the peer in a custom payout PSBT are not verified yet. A PSBT without valid
  signatures is refused, not treated as proof of a modified client (TODO in
  `finalize_custom_payout`).
- `TradeProtocolClient.java` was changed to talk to two daemons. It was compiled, not run.

### WP2 Wallet integration for trades (estimate: 3 to 4 weeks; issues #59, #185, #186, #187, #202, #229, #230, #232)

- One shared `BMPWallet` in `musigd` for the wallet service and the trade engine (#185).
- Key shares from the seed (D10): a method on `ProtocolWalletApi` that returns the key shares for
  a trade id, following `key-derivation.md`. The seed does not leave the `wallet` crate. The
  random key share in `protocol/src/multisig.rs:73` is removed; nonce generation stays random.
- Reserve the deposit inputs when the half PSBT is created. Release them when the trade is
  abandoned. Record the deposit transaction in the wallet at broadcast (F13, F14).
- Never issue a trade key or trade address twice (F12). Persist what was issued.
- Import the payout key at close with `deposit_payout_tap_tree`. Give the new sub-wallet a start
  checkpoint at the deposit height, so that no scan from the genesis block starts (F5, F19, #202).
- Imported coins are spent first in every transaction (D8, F15): keep the rule, specify it, and
  define what happens when the caller selects coins by hand.
- Drain: `drain_imported_balance` returns an unsigned PSBT today. Add signing, recording and
  storing of the drain transaction, as the wallet part of the `close` method (D8).
- Fee bumping: build a child transaction that spends an anchor output or an own unconfirmed
  output; replace-by-fee for wallet payments, the deposit abort (D13) and the claim (D3) (F20,
  #187). Rebroadcast and abandon are in pull request #235.
- Remove the panic in `sign` (F16). Encrypt the databases of imported keys or store their data in
  the main database (F17).
- Implement the lock methods of the mock wallet, or delete the mock after WP1.

Done when: a trade on regtest reserves, spends and releases real coins, both wallets can spend
their payout after the close, and the same seed gives the same key shares for the same trade id.

### WP3 Chain access for the engine (estimate: 4 to 6 weeks; issues #61, #188, #190, #196)

D2 makes the compact block filter client the only backend. This package is on the path to the
first milestone, and it contains the largest technical uncertainty of the plan.

- Extend the `chain` crate with one interface for: broadcast of a transaction or of a package of
  parent and child, current height, confirmation depth of a transaction, the transaction that
  spends a given outpoint, and notification of reorganisations.
- Implement it with one long-running `bip157` client that the wallet sync and the engine share
  (F28). The parts that the library offers: filters as events (`Event::IndexedFilter`), blocks on
  request (`get_block`), header changes with reorganisations (`Event::ChainUpdate`), the chain tip
  (`chain_tip`), package submission (`submit_package`), rescan from a height (`rescan_from`).
  Whether `rescan_from` fits the restart case must be checked.
- Watching works by script: a filter matches a block that creates or spends an output with a
  watched script. The engine then loads the block and looks for the trade transactions.
- Per trade the engine watches: the deposit transaction, both payout outputs, both possible
  warning escrow outputs, and its own broadcasts.
- Broadcast without acceptance result (N2): state "sent", repeated sending, end on confirmation
  of the transaction or of a conflict.
- CPFP package relay over the peer-to-peer network is assumed to work (N3). Test J7 checks the
  assumption against bitcoind early in this package.
- Replace `BitcoinCoreChainApi` (F27). Remove the Bitcoin Core options of `musigd`.
- Sync loop with retry and backoff (F18, #190). Defined behaviour when no peer is reachable or no
  peer serves filters (#188). Check that wallet network and peer network match. Remove the panics
  in `from_hostnames` (F22).
- Replace the simulated streams of `PublishDepositTx` and `SubscribeTxConfirmationStatus`, and the
  constant block height in `InitTrade`.
- One method that returns the fee rate for the engine's own transactions (claim, CPFP children,
  drain, deposit abort). For now it returns a fixed 10 sat/vB and contains a TODO comment; the
  real source is supplied later (N1). No other code contains a fee rate for these transactions.

Done when: the engine reports confirmations, spends and reorganisations of trade outputs on
regtest, also after a restart with blocks mined while it was stopped, and `musigd` runs without
any Bitcoin Core RPC connection.

### WP4 Complete the trade flows (estimate: 4 to 6 weeks)

- Deposit: broadcast, 3 confirmations (D6), timeout and abort transaction (D13).
- Cooperative close: import keys (WP2). Refuse to release when the remaining time before the
  peer's warning lock time is below a safety margin (F6).
- Forced close: broadcast the swap transaction. The buyer detects any spend of the seller payout,
  recovers the key share and imports the key. The seller publishes the swap transaction
  automatically when the buyer does not answer in time (D7).
- Custom payout: broadcast, finalisation that can be repeated safely, and the recovery rules that
  the bisq2 specification lists as open ("Joint contract still to confirm").
- Dispute path: new RPCs to publish the warning transaction and to report dispute state. Reactions
  to a confirmed warning of the peer: penalty when the full key is known, otherwise redirect
  after 1 block (D4). Handling of two competing warning transactions. Implement all closure
  types.
- Claim by script (D3): remove the claim transaction from the signing rounds (two of the nine
  signing sessions) and the fields `buyersClaimTxInputNonceShare`,
  `sellersClaimTxInputNonceShare` and `peersClaimTxInputPartialSignature` from `rpc.proto`. Build
  the claim transaction at claim time: script path witness, own key share as signing key, input
  sequence equal to the claim lock time, current fee rate. `claimTxPayoutAddress` stays, because
  the penalty transaction and the custom payout use this address (`protocol/src/trade_model.rs:1364`,
  `:1794`). The protobuf change should reach bisq2 together with the changes of WP7.
- The engine enforces chain preconditions before it releases a signature or a key share (F9, D6).
- Protection after close (D8): watch closed trades while a payout is unspent; penalty when the
  peer publishes a warning. New method `close` on `BmpWalletService`
  (`rpc/src/bmp_wallet_service.rs:81`) and a `Close` RPC in `bmp_wallet.proto`: call
  `drain_imported_balance`, then sign, store and send the drain transaction. A second call
  creates no second drain. At start: check the stored drain transaction and send it again if
  needed (N5).
- One event stream per trade for Java: state changes, transactions sent and confirmed, deadlines
  as block heights, required user actions.
- Validate trade parameters, redirect receivers and the fee receiver from Java: limits, count,
  dust, sum, network (D5, D9). Minimum amounts (D14).

Done when: every outcome of section 4.2 can be reached through the engine on regtest, for both
roles, and the claim confirms through the script path.

### WP5 Persistence and recovery (estimate: 3 to 4 weeks)

- Durable, encrypted trade store. Rule: the new state is on disk before any signature, key share
  or signed transaction leaves the engine. The comment at `protocol/src/trade_model.rs:1688` already
  demands this for the deposit.
- Stored per trade: trade id, peer data, parameters, pre-signed transactions, adaptor signature,
  the peer's key share after close, state, reserved outpoints, sent transactions, drain
  transaction. The own key shares are derived when needed (D10) and are not stored.
- The set of all trade ids ever used is stored, so that no id is accepted twice (N6).
- Secret nonces are never stored. A trade that is still in `Init` is abandoned at restart. After
  `Deposit` no MuSig2 nonce is needed any more: the penalty transaction is signed alone, the claim
  uses a single key (D3), and the custom payout uses wallet keys.
- Start-up: load trades, register watches, scan the chain from the deposit height of each open
  trade for events that happened while the process was stopped, resume deadlines.
- Every RPC can be repeated with the same result, as the bisq2 specification requires.
- Remove key shares from log output (F7). Zeroise secrets in memory where the libraries allow it.
- Backup and restore procedure for the trade store, documented for users, with the limits of a
  restore from the seed alone (end of section 7.2).

Done when: stopping and starting `musigd` in every state leads to the same final result as an
uninterrupted run.

### WP6 Fees and mempool policy (estimate: 2 to 4 weeks; issue #26)

- Agreement of both peers on the deposit fee rate and the fee rate of pre-signed transactions,
  with sanity limits in the engine.
- CPFP for warning and redirect through their anchors, and for swap and penalty through their
  wallet output, as packages over the peer-to-peer network (N3). The claim needs no CPFP: its
  fee is chosen at claim time and it can be replaced (D3).
- Fee bump schedule without mempool view: the engine raises the fee after a number of blocks
  without confirmation, so that a redirect or penalty confirms before the claim lock time ends.
  The start value comes from the fee rate method of N1; the schedule raises it in steps.
- Redirect lock time 1 (D4) and the reaction to a replaced warning transaction.
- Review of anchor amount and transaction weights against current relay policy.

Done when: the pinning scenario of `pinningattack.md` fails in a regtest test, and a pre-signed
transaction with a too low fee confirms through CPFP.

### WP7 Interface and operation (estimate: 1 to 2 weeks for the Rust side; issues #33, #58, #180)

- Protobuf changes for WP4 and WP5, including the `Close` RPC of the wallet service (D8);
  structured error codes that separate permanent, retryable and unknown results.
- `musigd`: configuration of network, peers, data directory, deposit timeout and release safety
  margin (N9); no secrets on the command line. No authentication (D11); the listen address stays
  fixed to loopback.
- Wallet events for bisq2.
- Java work in bisq2, outside this repository but required: new protobuf, dispute states in the
  trade state machine, trade parameters and deposits from the contract instead of fixed values,
  confirmation handling, the `Close` RPC in bisq2's `wallet.proto` and its call when the user
  ends the program.

### WP8 CI, review and release (estimate: 1 to 2 weeks plus external review)

- CI for the whole workspace on every change; formatting check with nightly rustfmt; dependency
  audit; Java integration tests; a nightly job for slow tests.
- Independent review of the protocol and of the cryptographic code before mainnet.
- Trial phase on signet or testnet with real users.
- Packaging of `musigd` with bisq2; `musigd` starts and stops together with bisq2 (issue #128;
  section 12, answer 7).

Estimated total for the Rust work: 21 to 33 weeks for one developer, plus the tests of section 9
that are not already counted in a package (5 to 8 weeks). The decisions did not change the total:
WP3 grew because the CBF client is now the only backend, WP7 shrank because authentication and the
removal of `walletrpc.Wallet` left it. The largest uncertainties are the long-running CBF client
with script watching (WP3) and package relay over the peer-to-peer network (N3, assumed to work).
The fixed fee rate (N1) is a placeholder; the work for the real source is not in the estimate.

## 9. Test plan

### 9.1 What exists today

| Area | Tests | Limits |
|---|---|---|
| Builders, PSBT merge, scripts, state model, shuffle, engine operations | 44 unit tests in `protocol/src` (25 before WP1) | No tests for malformed input from the peer |
| Protocol with real chain | 8 tests in `protocol/tests/trade_model_integration_tests.rs` (since WP1; before: 10 tests of `BMPProtocol` without assertions on results) | The tests broadcast and mine themselves, because the engine has no chain access; no assertions on wallet balances; no reorganisation |
| Wallet | About 60 unit tests and 16 integration tests, including 4 tests of `drain_imported_balance` | No reorganisation, no replaced transaction, no fee bumping, no trade use |
| `wallet.Wallet` gRPC | Rust and Java tests | Not related to trades |
| `musigrpc.Musig` gRPC | 28 tests in `rpc/tests/test_musig_service.rs` with mock wallets, 1 smoke test of the `musigd` binary (since WP1); a Java `main` without assertions | No chain; see F23 |

### 9.2 Test levels

| Level | Content | Location |
|---|---|---|
| L1 Unit | Pure logic without chain: builders, validation of peer messages, state transitions, key derivation, store round trips | Inside the modules of `protocol` and `wallet` |
| L2 Engine | Two engines with real `BMPWallet`s. Each engine uses the CBF client against a bitcoind on regtest. | `protocol/tests/` |
| L3 Service | Two in-process `musigd` services, driven by a Rust gRPC client | `rpc/tests/test_musig_service.rs` |
| L4 Java | A small set of Java tests for the interface | `rpc/src/test/java` |

Most scenarios belong to L2. L3 repeats one scenario per outcome to test the RPC layer. The trade
parameters of D5 allow small lock times in tests, so all tests are fast enough for the normal test
run. The warning lock time in a test must be larger than `MIN_CONFIRMATIONS`.

The engine under test has no RPC access to bitcoind (D2). Only the test code uses bitcoind RPC:
to mine, to read the mempool, and to submit a transaction directly when a test needs the node's
answer.

### 9.3 Test support code to build first

A harness in `protocol/tests/common/`:

- `Party`: engine, wallet in a temporary directory, trade store, CBF client. It can be stopped
  and started again from its store, and it can be created again from the seed alone.
- `Link`: carries messages between the two parties. A test can deliver, drop, delay, repeat or
  change each message.
- `TradeSim`: creates two funded parties and runs the trade up to a named stage, for example
  "deposit confirmed" or "seller released".
- Assertions: wallet balance of a party, trade state, a transaction is in the mempool, is
  confirmed, or spends a given outpoint.
- Conservation check at the end of every test: funds of both wallets, plus outputs to third
  parties, plus fees, equal the funds at the start.

Additions to `testenv`:

- Reorganise the last n blocks; mine until a relative lock time is reached.
- Mempool queries; the transaction that spends an outpoint; confirmation count.
- Direct submission of a transaction, to read the node's reject reason.
- A raised minimum relay fee to simulate a fee market.
- Stop and start bitcoind inside one test; disconnect the peer connection of a client.
- Fund P2WPKH coins and coins with many small outputs.

Derived key shares (D10) make the keys of a test reproducible from the test seed. A deterministic
random source for nonces makes L1 tests fully reproducible. The TODO comment in
`protocol/src/multisig.rs:227` already asks for it.

Scenarios marked "both roles" run once with the buyer and once with the seller as the acting
party. L3 tests also cover maker and taker.

### 9.4 Test catalogue

"Node rejects" means: the test submits the transaction to bitcoind directly and checks the reject
reason. "Engine refuses" means: the engine returns an error and sends nothing.

#### A. Setup and deposit

| ID | Scenario | Expected result |
|---|---|---|
| A1 | Complete setup, deposit confirms | State `Deposit` is reported as confirmed only at 3 confirmations; deposit has the agreed outputs; both wallets are reduced by deposit and fee share |
| A2 | Peer stops answering in each setup step before the deposit is signed | Trade abandoned; reserved coins released; no funds moved |
| A3 | One trader signs the deposit, the peer never publishes it | After the timeout the trader spends his own input (D13); a later deposit is rejected by the node |
| A4 | Both traders publish the deposit | Both engines reach the same state after the confirmation |
| A5 | Deposit stays unconfirmed because of a low fee | Fee is raised through a change output, or the abort of A3 is used; the engine follows the transaction that confirms |
| A6 | Wallet has too few funds | Clear error in the nonce step; nothing reserved |
| A7 | Deposit funded by many inputs, by P2WPKH inputs, and without a change output | Valid deposit; fee rate within the tolerance |
| A8 | Two trades start at the same time from one wallet | No coin is used twice |
| A9 | Trade with a fee receiver output (D9) | Output exists with the exact amount; paid by the seller's half |
| A10 | Amount below the minimum (D14) | Rejected before any coin is reserved |
| A11 | Peers use different fee rates, lock times or amounts (D5) | Setup fails with an error before the deposit is signed |
| A12 | Trade parameters outside the limits: redirect lock time not 1, warning lock time not larger than `MIN_CONFIRMATIONS` plus margin, zero claim lock time | Rejected in the request that carries the parameters, before a coin is reserved. Since WP1 this is `GetNonceShares` (open point O13 of the specification). |
| A13 | The peer's deposit input does not exist or is already spent (D12: not checked) | Deposit never confirms; abort as in A3; own coins are free again |
| A14 | Swap signature or key share requested at 0, 1 and 2 confirmations (both roles) | Engine refuses; succeeds at 3 (D6, F9) |
| A15 | Deposit timeout and release safety margin set to other values in the configuration | The engine uses the configured values; without configuration it uses the defaults (N9) |

#### B. Cooperative close

| ID | Scenario | Expected result |
|---|---|---|
| B1 | Buyer starts payment, seller confirms, keys are exchanged | Both `TradeClosed(Cooperative)`; buyer's wallet shows `T + Db`, seller's shows `Ds`; no further transaction on chain |
| B2 | Both spend the payout to a wallet address | Transactions confirm; balances correct |
| B3 | Seller asks for release before the buyer started the payment | Refused; no key share returned |
| B4 | Buyer asks for the seller's key share in an earlier state | Refused |
| B5 | Seller releases too close to the warning lock time (F6) | Refused, or the swap transaction is published at once, as specified |
| B6 | Next deposit or payment after a close, with the payout still unspent | The imported payout is spent first (D8) |

#### C. Forced close with the swap transaction

| ID | Scenario | Expected result |
|---|---|---|
| C1 | Seller releases, buyer does not answer, seller's engine publishes the swap transaction on its own (D7) | Seller receives `Ds` minus fee; state `TradeClosed(Forced)` |
| C2 | Buyer is offline during C1 and starts later | Buyer finds the spend in a block, recovers the key, imports it and owns `T + Db` |
| C3 | Seller publishes the swap transaction without sending the key share | Same result as C2 |
| C4 | Seller tries to build the swap transaction before the buyer started the payment | Not possible; the buyer's partial signature is missing |
| C5 | Swap transaction with a too low fee | Confirms through CPFP from the seller's output |
| C6 | Swap transaction is unconfirmed when the buyer's warning lock time ends, and the buyer publishes his warning | Engine reacts as specified (redirect) |
| C7 | The seller payout is spent by a transaction that is not the expected swap transaction | Buyer still recovers the key from the signature |

#### D. Custom payout after mediation

| ID | Scenario | Expected result |
|---|---|---|
| D1 | Both sign, transaction is published | Outputs as mediated; fee split in half; `TradeClosed(Custom)` |
| D2 | Extreme splits: everything to one side; a side below the dust limit | Valid transaction, or a clear error |
| D3 | Only one side signs | No valid transaction; trade stays `CustomPayoutSigned`; the warning path is still available after the lock time |
| D4 | Finalisation is called twice, and again after a restart | Same transaction both times |
| D5 | Peer PSBT with changed outputs or amounts | Rejected |
| D6 | Custom payout while the seller already published the swap transaction | One of the two confirms; the engine follows the confirmed one |

#### E. Warning, redirect and claim (both roles)

| ID | Scenario | Expected result |
|---|---|---|
| E1 | Warning before its lock time | Engine refuses; node rejects (`non-BIP68-final`); state unchanged |
| E2 | Warning after the lock time, peer's engine answers with redirect when the warning has 1 confirmation | Burning men receive the listed amounts; both traders receive nothing; state `...Redirect` on both sides |
| E3 | Warning after the lock time, peer silent, claim after the claim lock time | Claim spends the escrow through the script path (D3); publisher receives everything minus fees; state `...Claim` |
| E4 | Claim before the claim lock time | Engine refuses; node rejects |
| E5 | Redirect after the claim lock time, while the claim is not yet confirmed | One of the two confirms; both engines end in the matching state |
| E6 | Both traders publish their warning | One confirms; the other trader's engine switches to the reaction for a peer warning |
| E7 | Peer warning confirms while the trader is offline; the trader starts before the claim lock time ends | Redirect is published in time |
| E8 | As E7, but the trader starts after the claim was confirmed | Engine reports the loss and closes the trade |
| E9 | Warning with a too low fee | Confirms through CPFP on the anchor |
| E10 | Redirect with a too low fee, close to the end of the claim lock time | Fee bump schedule gets it confirmed first |
| E11 | Claim script details (L1 and L2): the witness is a script path spend with the claimer's key share; a spend with the peer's key or with a lower input sequence is rejected by the node | As described |
| E12 | Many redirect receivers; a receiver below the dust limit | Standard transaction, or rejected in setup |
| E13 | Warning published after a custom payout or swap transaction spent a payout | Engine refuses; node rejects; trade stays closed |
| E14 | Redirect in the same block as the warning (D4) | Node rejects; the engine waits for 1 confirmation |
| E15 | Claim with a low fee stays unconfirmed | Engine replaces it with a higher fee (D3) |
| E16 | The setup messages contain no claim nonce and no claim signature (D3) | Checked on the message types |

#### F. Penalty (both roles)

| ID | Scenario | Expected result |
|---|---|---|
| F1 | After a cooperative close the peer publishes his warning | Penalty transaction takes the full escrow; state `...PenaltyTx` |
| F2 | As F1, after a forced close | Same for the buyer |
| F3 | As F1, while the trader is offline, start before the claim lock time ends | Penalty in time |
| F4 | The trader spent his payout before the peer's warning lock time ended | Peer's warning is invalid; nothing to do |
| F5 | Penalty with a too low fee | Confirms through CPFP |
| F6 | The engine does not hold the full key and is asked for a penalty | Refused |

#### G. Invalid or hostile peer messages (L1, each one a separate test)

| ID | Scenario | Expected result |
|---|---|---|
| G1 | Key share is not a valid point; both key shares equal; key share equal to our own | Error, no panic |
| G2 | Half PSBT: wrong placeholder amount, no `witness_utxo`, non-segwit input, sequence without RBF flag, more than 126 inputs, one of our coins as input, fee below target | `InvalidPsbt` |
| G3 | Half PSBT with extra global or per-input fields | Rejected |
| G4 | Invalid nonce; nonce repeated from another signature | Error |
| G5 | Invalid partial signature for each pre-signed transaction | Aggregation fails; deposit is not signed |
| G6 | Partial signature for a different message (other outputs or lock time) | Aggregation fails |
| G7 | Swap sighash from the seller that belongs to another transaction | Covered by the written security argument of WP0; the test proves that using the signature still reveals the key share |
| G8 | Address for the wrong network; non-standard script as address | Error |
| G9 | Deposit PSBT from the peer with a changed output or a foreign input | Combine fails |
| G10 | Wrong private key share at close | `MismatchedKeyPair`; state unchanged |
| G11 | Oversized messages and lists | Rejected by size limits |
| G12 | Any message repeated or delivered in the wrong order | Same result as the first delivery, or an error; never a second signature with the same nonce |
| G13 | Data of the peer that an unmodified client never produces, for each check that the specification lists; then the corrected data (D17) | Trade terminated; the reason is in the log with its details and contains no secret; the corrected data is refused; the state does not change |
| G14 | A request is refused by a precondition or by a failed check (D18) | The trade holds no data and no signature of this request |
| G15 | A request is refused for a reason that can have another cause: too early, wrong order, wrong role, invalid data of the caller, no network peer; then the request is sent again when the reason is gone (D17) | The trade is not terminated; the repeated request succeeds |

#### H. Restart, persistence and restore

| ID | Scenario | Expected result |
|---|---|---|
| H1 | Restart in state `Init` | Trade abandoned; coins released |
| H2 | Restart in each later state, then continue | Same final result as without restart |
| H3 | Process stops between "state stored" and "response sent" for each RPC | Repeating the RPC returns the same response |
| H4 | Process stops before the state is stored | No signature or secret has left the process |
| H5 | Chain events happen while the process is stopped (swap, warning, redirect) | Found at start-up; correct reaction |
| H6 | Trade store opened with a wrong password; damaged store | Clear error; no partial load |
| H7 | Secret nonces and the own key shares are not part of the stored data | Checked on the stored bytes |
| H8 | Wallet locks and trade store agree after a crash at any point | No coin stays reserved without a trade; no trade without its reservation |
| H9 | Key derivation (D10): same seed and trade id give the same key shares; other trade id, other seed or other role label give different ones; the test vectors of the specification pass | As described |
| H10 | Derived key shares differ from every key of the BIP86 wallet in the first 1000 addresses | No collision |
| H11 | Restore from seed plus a backup of the trade store, in each state after `Deposit` | Trade continues to the same result |
| H12 | Restore from seed only, open trade | Own key shares are derived again; a cooperative close with the peer succeeds; the engine reports that unilateral exits are not available |
| H13 | Two signing sessions of the same trade never use the same nonce, also after a restart | Checked on the nonce values |

#### I. Wallet interaction

| ID | Scenario | Expected result |
|---|---|---|
| I1 | 100 trades in sequence from one wallet | No address or internal key is issued twice (F12) |
| I2 | Payout key import after close, then restart of the wallet | Payout visible and spendable |
| I3 | `send_to_address` during an open trade | Reserved coins are not used |
| I4 | Sign a PSBT without `witness_utxo` | Error, no panic (F16) |
| I5 | Anchor outputs after an unused warning path | Not shown as spendable funds, or handled as specified |
| I6 | Files on disk after a trade | No unencrypted file with trade data (F17) |
| I7 | Import of a payout key at a known deposit height | The next sync starts at or after that height, not at the genesis block (F19) |
| I8 | Fee rate of the transactions that the engine builds on its own: claim, CPFP child, drain, deposit abort | Each one pays the rate that the fee rate method returns, 10 sat/vB for now (N1) |

#### J. Chain conditions

| ID | Scenario | Expected result |
|---|---|---|
| J1 | Reorganisation lowers the deposit depth below 3 before the payment started | State goes back to "unconfirmed"; no secret was released |
| J2 | Peer double-spends his deposit input after one confirmation, with a reorganisation | Trade ends as abandoned; own coins released |
| J3 | Reorganisation removes a confirmed warning | Lock time counting restarts; no early claim or redirect attempt |
| J4 | Reorganisation removes a confirmed swap transaction | Engine sends it again |
| J5 | No peer reachable during a required broadcast | Retries; state unchanged; success after a peer returns |
| J6 | Pinning scenario of `pinningattack.md` | With D4 the attack transactions are rejected by the node |
| J7 | Mempool minimum fee above the pre-signed fee rate | Package of parent and child, sent over the peer-to-peer network, is accepted (N3) |
| J8 | Sync error during a trade | Sync continues after backoff (F18) |
| J9 | The engine sends a transaction that the node does not accept | State stays "sent"; nothing is reported as confirmed; repeated sending follows the schedule (N2) |
| J10 | The only peer does not serve block filters | Clear error state; no trade step that needs the chain proceeds |
| J11 | `musigd` runs a full trade with no Bitcoin Core RPC configured | Passes (D2, F27) |

#### K. Interface misuse

| ID | Scenario | Expected result |
|---|---|---|
| K1 | Every RPC in every state for both roles | Allowed transitions succeed, all others return an error; generated from the transition table in `state.rs` |
| K2 | Unknown trade id; duplicate `InitTrade`; `InitTrade` with the id of a closed trade | Error; existing trade untouched (F2, N6) |
| K3 | Seller-only RPC called by the buyer and the reverse | Error |
| K4 | Listen address of `musigd` | Loopback only; no option binds another address (D11, N7) |
| K5 | Log output at trace level during a full trade | Contains no key share, seed or password (F7) |

#### L. Service level (L3 and L4)

| ID | Scenario | Expected result |
|---|---|---|
| L1 | Cooperative close through gRPC with real wallets and chain, for the four role combinations | As B1 |
| L2 | One test per other outcome: forced, custom, redirect, claim, penalty | As in C, D, E, F |
| L3 | Restart of one `musigd` in the middle of L1 | As H2 |
| L4 | Event stream content for L1 and L2 | Events in the specified order with correct heights |
| L5 | Java test: cooperative close and one dispute through the real protobuf classes | Passes in CI |

#### M. Protection after close (D8)

| ID | Scenario | Expected result |
|---|---|---|
| M1 | `Close` is called with unspent payouts of closed trades | One drain transaction to an address of the seed wallet is stored and sent; after it confirms, the imported balance is zero |
| M2 | After M1 the peer publishes his warning | Node rejects it, because its input is spent |
| M3 | `Close` while another trade is open and its coins are locked | Locked coins are not drained; the open trade continues after the restart |
| M4 | The drain transaction is not confirmed at the next start | Sent again, or replaced with a higher fee; exactly one drain confirms |
| M5 | Process is killed without a `Close` call; the peer's warning confirms; start before the claim lock time ends | Penalty in time (as F3) |
| M6 | As M5, start after the peer's claim confirmed | Engine reports the loss; documented risk (N5) |
| M7 | `Close` with nothing to drain, or with an amount below the fee | `Close` returns without error; no transaction |
| M8 | No peer reachable at `Close` | Drain transaction is stored and sent at the next start |
| M9 | `Close` is called twice; `Close` is called while no wallet is open | One drain transaction only; clear result for the second case |
| M10 | The program keeps running after a trade close; the peer publishes his warning late | Penalty (as F1); no drain is needed |
| M11 | `Close` through gRPC (L3) | The `Close` RPC of `wallet.Wallet` gives the same result as M1 |

### 9.5 Order of test work

1. Now, without waiting for other work: characterisation tests for `TradeModel` with the existing
   mock wallets (parts of B, C, D, G, K at level L1 and L3). They replace the Java `main` program
   as the only driver of the trade service and protect the move in WP1.
2. With WP1 and WP2: the harness of 9.3, groups A (without chain timing), B, I, and H9, H10.
3. With WP3: J5, J7, J9, J10, J11 and the chain parts of A.
4. With WP4: groups C, D, E, F, M and the rest of J.
5. With WP5: group H.
6. With WP6: C5, E9, E10, E15, F5, J6.
7. With WP7: K4, K5, L.

## 10. Milestones

| Milestone | Content | Packages |
|---|---|---|
| M1 Regtest trade, cooperative | One engine, real wallets, derived key shares, real deposit over the CBF client, 3 confirmations, key import, balances checked | WP0, WP1, WP2, core of WP3 |
| M2 All exits on regtest | Forced close, custom payout, warning, redirect, claim by script, penalty, drain at close | Rest of WP3, WP4 |
| M3 Restart safe | Trade store, recovery, repeatable RPCs, restore | WP5 |
| M4 Fee safe | CPFP over the peer-to-peer network, pinning countermeasures, agreed parameter values | WP6 |
| M5 Integrated with bisq2 on regtest | Interface complete, Java state machine | WP7 |
| M6 Signet trial | CI complete, external review started, trial with users | WP8 |
| M7 Mainnet | Review findings closed, trial results accepted | - |

Compared with revision 1, M1 now needs the CBF chain access, because no Bitcoin Core
implementation exists as a first step (D2).

## 11. Not part of this plan

- Silent payments and fee channels for trade fees (`tradefees/`, `silentpayment/`).
- External watchtowers.
- Hiding the IP address towards the peers, for example with Tor (issues #127, #129; N4).
- A second chain backend. The chain interface of WP3 allows one later.
- Authentication for the gRPC port (D11).
- A real fee rate source. The engine uses a fixed fee rate until it is supplied (N1).
- Fixed values for lock times, deposit timeout and release safety margin. They are configurable
  (N9).
- A check of the peer's deposit inputs on chain (D12).
- The Java implementation in bisq2, except as a listed dependency.

## 12. Questions and answers, to be reviewed later

These were the open questions of revision 2. The project owner answered them on 2026-11-17. Row 8
was added on 2026-11-18. The questions stay here so that the answers can be reviewed later. The column "Review later" names
what should be checked at that review.

| # | Question | Answer (2026-11-17) | Where it is in the plan | Review later |
|---|---|---|---|---|
| 1 | What does "when the wallet closes" in D8 mean: the shutdown of `musigd` and the closing or locking of the wallet, or the close of each trade? | `BmpWalletService` gets a new method `close`. Java calls it when the user ends the program. The method calls `drain_imported_balance`. | D8 in section 7.1; N5; WP2, WP4, WP7; test group M | The drain does not run when the program ends without the `close` call (crash, kill, power loss). Does `close` also end other work, for example the sync loop or open trades? |
| 2 | Which payment methods need which lock times, and which values do the deposit timeout (D13) and the release safety margin (F6) get (N9)? | These values must be configurable. | N9; WP1, WP7; tests A11, A12, A15 | The default values, and the limits that the engine enforces. Both peers must use the same lock times. |
| 3 | Who supplies the fee rate for transactions that the engine builds on its own (N1)? | It is supplied later. For now one method returns a fixed value of 10 sat/vB and contains a TODO. | N1; WP3, WP6; test I8; section 11 | The real fee rate source. With a fixed 10 sat/vB the claim, drain, abort and CPFP transactions can stay unconfirmed when the network needs a higher rate. |
| 4 | Does package relay of a low-fee parent with its child work over the peer-to-peer network with the nodes that users will connect to (N3)? | Assume yes. | N3; WP3, WP6; test J7 | The result of test J7. It answers the question for bitcoind 29.0 only, not for all nodes that users connect to. |
| 5 | Is the derivation rule of N6 acceptable, including the rule that a trade id can never be used twice? | Yes. | N6; WP2, WP5; tests H9, H10, K2 | The exact derivation path and the test vectors, when `key-derivation.md` is written. |
| 6 | Is the reorganisation rule of N8 acceptable: reactions at 1 confirmation, final at 3? | Yes. | N8; WP4; tests A14, J1 to J4 | Nothing specific. |
| 7 | Does `musigd` keep running when bisq2 is closed? | No. | D7 and the paragraph after the table in section 7.1; N5; WP8 | Nothing is watched while bisq2 is closed. For closed trades the drain at `close` is the only protection in that time. For open trades the user must start the program before the claim lock time ends after a peer warning. Is this acceptable for mainnet, or is a watching process needed later? |
| 8 | Which actions does a terminated trade (D17) keep after the deposit is signed? Its funds are then locked in the deposit outputs. | Postponed on 2026-11-18. To be decided later. | D17; open point O1 of `docs/specifications/trade/README.md`; `trade-states.md`, section "Terminated trades" | The decision itself. Until then the specification states: the cooperative steps end, and the engine keeps every action that the trader can perform alone (warning, redirect, penalty, claim, swap transaction, reading the key from a spend, deposit abort). Without such a rule a terminated trade would lose its funds. |

Points of section 7.2 that were not part of the questions and still need a decision in WP0: N2
(broadcast without result), N4 (peers and the visible IP address), N7 (wording of the security
assumption for the open gRPC port).
