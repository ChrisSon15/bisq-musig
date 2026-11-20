# Regular shutdown of `musigd`

`musigd` runs only while bisq2 runs. This file defines how the process ends in the regular way,
and what its caller can rely on.

## The request to end

`musigd` ends in the regular way when it is asked to. There are two ways to ask:

- **The request `Shutdown`.** This is the way that a caller uses. The process answers the
  request, and then ends as described below. The response means that the request was accepted.
  It does not mean that the process has ended; the caller waits for the end of the process.
- **A signal.** The signals SIGTERM and SIGINT have the same effect as the request. On a
  platform without these signals, Ctrl-C of the console of the process has it. This way exists
  for a user who runs `musigd` in a terminal and for the tools of the operating system.

The request `Shutdown` is not part of the official interface of `musigd`, which consists of the
trade service and the wallet service. It belongs to a separate, internal service
(`musigd.internal.Daemon`), which the process serves on the same port. The internal service can
change without notice.

Rules:

- No request of the trade service or of the wallet service ends the process. The close operation
  of the wallet service ([post-trade-protection.md](post-trade-protection.md)) does not end it,
  and the wallet stays open after it.
- The request `Shutdown` needs no open wallet and no password. The security assumptions of the
  other requests apply ([service-interface.md](service-interface.md)): every local process can
  send it.
- A further request to end, in either way, does not change the shutdown that runs. A further
  request `Shutdown` can get an error instead of a response, because the process accepts no new
  request.

The project owner decided the two ways on 2026-11-20 (O21 in [README.md](README.md)).

## What the process does

1. From the request to end on it accepts no new connection and no new request.
2. It ends the subscriptions. The stream of a subscription ends without an error. The
   subscriptions are the requests `SubscribeWalletEvents`, `SubscribeTradeEvents` and
   `SubscribeTxConfirmationStatus`, and the stream of confirmation events that `PublishDepositTx`
   returns.
3. A request that runs at this moment is completed, and its caller gets the response. The
   request `Shutdown` itself is such a request.
4. The process waits until all running requests have their responses, but not longer than
   10 seconds. A request that is still open after this time is ended.
5. It ends its connections to the peers of the Bitcoin network.
6. It ends with the exit status 0.

A process that could not start, or whose server failed, ends with another exit status.

Rationale for step 2: a subscription has no result that its caller waits for, and a caller keeps
its subscriptions open for as long as the process runs. If the process waited for the caller to
close them, every regular shutdown would last until the limit of step 4.

Rationale for the limit of step 4: a request must not be able to prevent the end. An example is
a caller that starts a request and never sends its content.

## What the shutdown does not do

- **It does not drain the imported coins.** The drain is the close operation of the wallet
  service, which needs a running process. A caller that ends the program for the user therefore
  first sends the close request, waits for its response, and then sends the request `Shutdown`.
- **It is not needed to keep data.** Every change of a trade or of the wallet is stored when it is
  made ([persistence-and-recovery.md](persistence-and-recovery.md)). A process that is killed
  loses no stored state. This also holds for a request that the limit of step 4 ends.

What the regular shutdown adds to a kill: no request is cut off in the middle, so the caller
knows the result of every request that it sent, and the exit status tells the caller that the
process ended as intended.
