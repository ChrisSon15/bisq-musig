# Specification of the wallet functions for trades

These files define what the wallet must do for the trade engine. They describe the required
behaviour and the reasons. The behaviour of the trade protocol itself is specified in
[../trade/README.md](../trade/README.md).

| File | Content |
|---|---|
| [trade-keys.md](trade-keys.md) | Keys and addresses that the wallet issues to a trade; key shares |
| [coin-reservation.md](coin-reservation.md) | Reservation of the coins of a deposit; recording of transactions that the wallet did not build |
| [imported-keys.md](imported-keys.md) | Import of the key of a payout, start of its scan, spending order, drain, storage |

Findings and decisions refer to `concept/CompletionPlan.md` (F12 to F19, D8, D10).

## Open points

Each of these points is a rule that this specification states, but that the project owner has not
confirmed.

| # | Question | Rule in the specification | File |
|---|---|---|---|
| W1 | A wallet that is restored from its seed words must find funds on keys of trades that lie behind a long row of unused keys. | Not specified. The wallet has no restore from seed words yet. The rule belongs to that function. | [trade-keys.md](trade-keys.md) |
| W2 | Coins that stay reserved when `musigd` stops during the setup of a trade. | They stay reserved until the trade store exists (work package WP5 of the plan). Then the engine releases them at the next start. | [coin-reservation.md](coin-reservation.md) |
| W3 | Should the wallet service offer requests to list and release reserved coins by hand? | No such request exists. | [coin-reservation.md](coin-reservation.md) |
| W4 | May the caller select the coins of a payment by hand, and does the rule "imported coins first" then still hold? | No request lets the caller select coins. If one is added, the imported coins that are not reserved are still added to the transaction. | [imported-keys.md](imported-keys.md) |
