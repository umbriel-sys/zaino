# Logging

For operators. Logging is configured from the environment when `zainod`
starts; the developer side is in
[`zaino-logging`'s guide](../packages/zaino-logging/usage.md).

## Environment variables

| Variable | Values | Default | Effect |
| --- | --- | --- | --- |
| `RUST_LOG` | a tracing filter | `zaino=info,zainod=info,zainodlib=info` | which targets log, at which level |
| `ZAINOLOG_FORMAT` | `stream`, `tree`, `json` | `stream` | output format |
| `ZAINOLOG_COLOR` | `true`, `false`, `auto` | `true` | ANSI colour; `auto` detects a terminal |
| `ZAINOLOG_LOCATION` | `true` | off | adds each event's source `file:line` |

Unset, `RUST_LOG` shows Zaino's own crates only. `RUST_LOG=info` includes
every crate, zebra's among them. A panic is always logged, as an error event
at its origin, whatever the filter.

## Formats

| Format | Shape | Written to |
| --- | --- | --- |
| `stream` | events in time order, each with its time, level, target and fields | standard output |
| `tree` | events nested under their spans, for following one request or one sync step | standard error |
| `json` | one JSON object per line with an RFC 3339 timestamp, for log aggregation | standard output |

## Examples

```sh
zainod start                                              # stream, Zaino's crates at INFO
RUST_LOG=zaino=debug,zainod=debug,zainodlib=debug zainod start
RUST_LOG=info zainod start                                # every crate, zebra included
RUST_LOG="zaino_service=debug,zebra_state=warn" zainod start
ZAINOLOG_FORMAT=json zainod start >> /var/log/zainod.json
ZAINOLOG_FORMAT=tree zainod start
RUST_LOG=warn zainod start
```

The same variables apply to the tests:

```sh
RUST_LOG=debug ZAINOLOG_FORMAT=tree cargo nextest run
```
