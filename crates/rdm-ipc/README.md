# rdm-ipc

Linux-only shared protocol, bounded framing, runtime socket ownership, and async
client for RDM. There is no dependency on Iced or the download engine.

See [service architecture and limitations](../rdm-service/README.md).

Wire format: a big-endian u32 byte length (1..=1 MiB), followed by a JSON
`Packet { version, body }` (internal version 6; matching Rust binaries required). Exactly one request is sent per connection. A List,
Add, AddCli, Pause, Cancel, Remove, Retry or Shutdown receives one response; Subscribe receives an initial
snapshot followed by snapshots until disconnect. JSON parse errors never echo
untrusted payload text. Version mismatch is an explicit response.

Public snapshots intentionally omit source URLs. Add and AddCli Debug redact
input. Treat serialized requests as secrets; never log raw frames. AddCli carries
CLI-resolved absolute output, connections and explicit private-network opt-in;
GUI/browser Add continues to use service defaults. Both enter the same actor
admission and engine path.

`request_at` sends a mutation once. Failures after connecting may be ambiguous,
so callers must not automatically retry them. `ensure_running` retries only
read-only startup probes. Pause, Cancel, Remove, Retry and Shutdown must use the instance ID from the
current snapshot, not a saved job ID from an earlier service process.

`Job.queued` explicitly distinguishes waiting from running; `active()` means
unfinished (either queued or running), `running()` excludes queued work. Queued
inputs/URLs are private to the service and never appear in snapshots. Request
acknowledgements include the new job as the last record atomically. Remove
forgets queued or terminal records and private input durably before ACK/events,
without deleting files. Cancel retains retryable Cancelled history. Removal of
running/draining work is rejected, including queued-to-running promotion races.
Attached waiters treat absence as non-success with completion unconfirmed.
The actor bounds the total record set and never evicts unfinished work.

`Finished::Interrupted` represents held recovered work: it is neither queued nor
running. `Job.retryable` advertises private retry input, never exposes it.
Retry requires a current instance handle, rejects running/successful jobs and
creates a new ID at the tail. The response has this job last. Remove
accepts queued jobs and every terminal kind (including held Paused/Interrupted,
failed, cancelled, successful and retired attempts) without starting an engine.
It never reuses IDs or restores retry input; missing records return NotFound. Mutations are
durable before ACK; lost replies still require inspection, not blind retries.

Retried source records retain their original result and `retried_as` link, but
lose private input and retryability. The fresh attempt is appended with a new ID
in the same transaction; old handles/waiters cannot cancel or claim its result.
At capacity the retired source may be evicted, never other held work.

`Pause` supports running or queued work. `Job.pausing` means committed pause intent
with a still-draining worker, so `running()` remains true until joined.
`Finished::Paused` is held and has no worker; `held()` protects both retryable
Paused and Interrupted records from eviction. Paused and Pausing recovery never
performs network requests. Duplicate Pause is idempotent; resume uses Retry and
creates a fresh handle, not reactivation of an old attempt. Cancel can override
Pausing; a real completed/failed engine result wins over a late pause request.
