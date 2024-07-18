# HTTPing

HTTPing allows you to "ping" an http endpoint indefinitely with timestamps.

## Installation

```
$ cargo install --git https://github.com/stianfro/httping.git
```

## Usage

```
$ httping
httping: missing URL
Usage: httping [URL]
```

```
$ httping https://example.com
HTTPing https://example.com
2024-07-18T16:34:08.210834+09:00 200 OK 218.083µs
2024-07-18T16:34:09.381571+09:00 200 OK 27µs
2024-07-18T16:34:10.625965+09:00 200 OK 78.666µs
2024-07-18T16:34:11.740563+09:00 200 OK 24.459µs
```
