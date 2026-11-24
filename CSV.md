# CSV files

## `scores.csv` written by the logger

One finished run per line, appended as scores arrive over BLE. The first four columns
are the device's CSV exactly as received (see `crates/firmware-common/src/scores.rs` in
esp-timer); the logger adds only its own receipt timestamp.

| column        | meaning                                                        |
| ------------- | -------------------------------------------------------------- |
| `seq`         | monotonic per-boot sequence number; also the BLE ack cursor    |
| `player_uuid` | scanned player card UUID, or `-` when no card was scanned      |
| `elapsed_us`  | measured run time, microseconds                                |
| `at_ms`       | milliseconds since node boot when the run ended (device clock) |
| `received_at` | local wall-clock time the logger stored the row (RFC 3339)     |

Join to players on `player_uuid` = `uuid`.

## `players.csv` updated via player-card

| column | meaning                            |
| ------ | ---------------------------------- |
| `uuid` | player card UUID (as scanned)      |
| `name` | display name                       |
| ...    | any further player info, any names |

Example:

```csv
uuid,name,team
af1d45ff-2a68-4abc-b0c0-bc875b30bc25,Trevor Nichols,Blue
```
