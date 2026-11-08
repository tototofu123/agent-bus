# Scheduled jobs

`bus schedule` posts a `task.offer` into a room on a cron expression.
It is the scheduling **mechanism** only: what an offer says is up to the
schedule. The daily digest is a schedule (see the example below), not
part of this feature — that is perihel-v2 #12.

## Schedule format

A schedule is a row in the broker's SQLite store (`schedules` table),
so it survives restarts and lives wherever the broker's database lives.
Fields:

| Field | Meaning |
|---|---|
| `name` | unique name, e.g. `morning-digest` |
| `room` | room the offer is posted into, e.g. `#repo:perihel` |
| `cron` | cron expression (see below) |
| `timezone` | `local` (default), `UTC`, or an IANA name like `Asia/Hong_Kong` |
| `text` | the offer's text |
| `payload` | the `task.offer` payload — must contain `files` as an array (an empty array is fine) |
| `enabled` | whether the tick considers it |
| `created_at_ms`, `last_fired_ms` | bookkeeping; `last_fired` records the *occurrence* time, not the wall clock |

Cron expressions accept the 5-field crontab form (`min hour dom mon
dow`, seconds assumed 0), the 6-field form (`sec min hour dom mon
dow`), the 7-field form (+ year), and the `@daily` / `@weekly` /
`@monthly` / `@yearly` shorthands. `0 8 * * *` and `0 0 8 * * *` both
mean 08:00 daily.

## CLI

```bash
# Register the scheduler identity once (see "The scheduler role" below).
bus register --agent scheduler@bus --harness scheduler

bus schedule add --name morning-digest --room "#repo:perihel" \
  --cron "0 8 * * *" --timezone Asia/Hong_Kong \
  --text "Daily digest: AI news, repo PRs/issues summary, plans" \
  --files ""
bus schedule list
bus schedule disable --name morning-digest
bus schedule enable --name morning-digest
bus schedule remove --name morning-digest
```

`--files` is a comma-separated list of globs for the `task.offer`
payload (`--files "src/**,docs/**"`); `--payload` takes a JSON object
of extra fields merged into the payload (it may not contain `files` —
use `--files` for that). The CLI validates the cron, timezone, and
payload locally, so a bad expression fails fast without a broker
round-trip; the broker validates again on `schedule.add`.

## Tick + external timer

The broker has **no internal timer**. An external timer runs one tick
a minute:

```bash
bus schedule tick
```

A tick loads every enabled schedule, and for each one whose next
occurrence after its last fire (or its creation, if never fired) is due
within the last minute, posts its `task.offer` (through the same send
path as `bus send`: same role and payload validation, same seq
assignment, fanned out to room members and live tails) and records the
occurrence as `last_fired`. The tick prints what fired and what was
skipped (with reasons); a malformed schedule is skipped and reported,
never crashing the tick and never blocking the other schedules.

The tick runs as the `scheduler@bus` identity by default
(`$AGENT_ID` or `--agent` overrides it). It needs that identity's
token, which `bus register` stores like any agent's.

### Timer setup

Windows Task Scheduler (the bus runs on Windows day to day):

```powershell
schtasks /create /tn "agent-bus tick" /tr "bus schedule tick" /sc minute /mo 1
```

systemd (a `busd` on Linux):

```ini
# /etc/systemd/system/agent-bus-tick.timer
[Unit]
Description=agent-bus schedule tick, every minute
[Timer]
OnBootSec=1min
OnUnitActiveSec=1min
[Install]
WantedBy=timers.target
# with a matching agent-bus-tick.service running `bus schedule tick`
```

cron:

```cron
* * * * * AGENT_ID=scheduler@bus bus schedule tick
```

### Idempotency

A schedule fires **at most once per minute, even if the tick runs
twice**: `last_fired` records the *occurrence* time, so a second tick
in the same minute finds its next occurrence in the future and does
nothing. The due-check, the append, and the `last_fired` update run
inside one transaction while the broker holds the store lock, so two
concurrent ticks cannot double-fire. And there is no catch-up: an
occurrence more than a minute old is missed, not fired late — if the
broker was down at 08:00, the 08:00 digest does not fire at 10:00.

## The scheduler role

The scheduler is a timer, not a model: it posts only user-authored
schedule entries and never reasons, summarises, or claims, so it
cannot honestly declare a model — yet it must post `task.offer`, which
needs the Conversation or Peer role. The broker grants harness
`scheduler` the Conversation role explicitly (`Role::assign` in
`crates/proto/src/lib.rs`), enough to post `task.offer` and nothing
more. It still may not send `policy` (human-only), and no other
harness's role rule changed. The match is exact: a harness merely
containing "scheduler" does not earn the role.

## Timezones

Cron expressions are evaluated in the schedule's timezone, so a job
follows the timezone it was written for no matter which machine the
broker runs on. `local` is the broker machine's local time;
`Asia/Hong_Kong` is an IANA name (any name `chrono-tz` knows). The
morning digest above fires at 08:00 Hong Kong time even if the broker
runs on a UTC machine.

## Example: the morning digest

```bash
bus schedule add --name morning-digest --room "#repo:perihel" \
  --cron "0 8 * * *" --timezone Asia/Hong_Kong \
  --text "Daily digest: AI news, repo PRs/issues summary, plans" \
  --files ""
```

The digest content itself is perihel-v2 #12; this schedule is only
the trigger that posts the offer each morning.
