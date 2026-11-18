# Specification of the wallet functions for trades

These files define what the wallet must do for the trade engine. They describe the required
behaviour and the reasons. The behaviour of the trade protocol itself is specified in
[../trade/README.md](../trade/README.md).

| File | Content |
|---|---|
| [trade-keys.md](trade-keys.md) | Keys and addresses that the wallet issues to a trade; key shares |
| [coin-reservation.md](coin-reservation.md) | Reservation of the coins of a deposit; recording of transactions that the wallet did not build |
| [imported-keys.md](imported-keys.md) | Import of the key of a payout, start of its scan, spending order, drain, storage |
| [events.md](events.md) | Events by which the wallet service reports a change of the wallet to its caller |

Findings and decisions refer to `concept/CompletionPlan.md` (F12 to F19, D8, D10).

## Open points

| # | Question | Rule in the specification | File |
|---|---|---|---|
| W5 | Which transport carries the events of the wallet? An earlier design note proposed a publish and subscribe socket (ZeroMQ). | A stream of the gRPC service, like the events of a trade. No second transport. | [events.md](events.md) |

## Decided points

These points were open. The project owner decided them on 2026-11-18.

| # | Question | Decision | File |
|---|---|---|---|
| W1 | A wallet that is restored from its seed words must find funds on keys of trades that lie behind a long row of unused keys. | Postponed. Restoring, in particular after a crash in the middle of a trade, is designed after the work packages of the plan are finished. Listed in `concept/CompletionPlan.md`, section 12, row 9. | [trade-keys.md](trade-keys.md) |
| W2 | Coins that stay reserved when `musigd` stops during the setup of a trade. | Accepted until the trade store exists (work package WP5 of the plan). The trade store exists since 2026-11-18: the engine abandons such a trade at the next start and releases its coins. | [coin-reservation.md](coin-reservation.md) |
| W3 | Should the wallet service offer a way to list and release reserved coins by hand? | Yes. `ListUtxos` marks the reserved coins, and `ReleaseUtxo` releases one. A coin that a trade known to the engine has reserved is refused. | [coin-reservation.md](coin-reservation.md) |
| W4 | May the caller select the coins of a payment by hand? | No. The wallet selects the coins of every transaction, so the rule "imported coins first" has no exception. | [imported-keys.md](imported-keys.md) |
